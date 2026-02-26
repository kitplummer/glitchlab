use std::collections::HashMap;
use std::sync::Arc;

use glitchlab_kernel::agent::Message;
use glitchlab_kernel::budget::BudgetTracker;
use glitchlab_kernel::error;
use glitchlab_kernel::tool::ToolDefinition;
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

use crate::chooser::ModelChooser;
use crate::provider::anthropic::AnthropicProvider;
use crate::provider::gemini::GeminiProvider;
use crate::provider::openai::OpenAiProvider;
use crate::provider::{Provider, ProviderError, ProviderInit, parse_model_string};
use crate::response::RouterResponse;

// ---------------------------------------------------------------------------
// Router — vendor-agnostic LLM routing
// ---------------------------------------------------------------------------

/// Routes LLM calls to the correct provider based on model string,
/// tracks budget, and handles retries.
pub struct Router {
    /// Role → model string mapping (e.g. "planner" → "gemini/gemini-2.5-flash-lite").
    routing: HashMap<String, String>,

    /// Provider name → provider instance.
    providers: HashMap<String, Arc<dyn Provider>>,

    /// Shared budget tracker (wrapped in Mutex for interior mutability).
    budget: Arc<Mutex<BudgetTracker>>,

    /// Optional cost-aware model chooser. When present, takes precedence
    /// over the static `routing` map for model resolution.
    chooser: Option<ModelChooser>,
}

impl Router {
    /// Build a router from a routing config and budget.
    ///
    /// Providers are lazily created from environment variables.
    /// Missing API keys are not an error until a model from that
    /// provider is actually requested.
    pub fn new(routing: HashMap<String, String>, budget: BudgetTracker) -> Self {
        Self::with_providers(routing, budget, HashMap::new())
    }

    /// Build a router with pre-resolved provider credentials.
    ///
    /// For each entry in `init_configs`, creates the appropriate provider.
    /// For known providers ("anthropic", "gemini", "openai") not present
    /// in `init_configs`, falls back to the existing `from_env()` behavior.
    pub fn with_providers(
        routing: HashMap<String, String>,
        budget: BudgetTracker,
        init_configs: HashMap<String, ProviderInit>,
    ) -> Self {
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();

        // Create providers from explicit init configs.
        for (name, init) in &init_configs {
            providers.insert(name.clone(), Arc::from(create_provider(init)));
        }

        // Fall back to env for known providers not in init_configs.
        if !providers.contains_key("anthropic")
            && let Ok(p) = AnthropicProvider::from_env()
        {
            providers.insert("anthropic".into(), Arc::new(p));
        }
        if !providers.contains_key("openai")
            && let Ok(p) = OpenAiProvider::openai_from_env()
        {
            providers.insert("openai".into(), Arc::new(p));
        }
        if !providers.contains_key("gemini")
            && let Ok(p) = GeminiProvider::from_env()
        {
            providers.insert("gemini".into(), Arc::new(p));
        }

        Self {
            routing,
            providers,
            budget: Arc::new(Mutex::new(budget)),
            chooser: None,
        }
    }

    /// Attach a cost-aware model chooser. When set, model resolution
    /// uses the chooser first and falls back to the static routing map.
    pub fn with_chooser(mut self, chooser: ModelChooser) -> Self {
        self.chooser = Some(chooser);
        self
    }

    /// Register a custom provider (e.g. Ollama, LM Studio).
    pub fn register_provider(&mut self, name: String, provider: Arc<dyn Provider>) {
        self.providers.insert(name, provider);
    }

    /// Resolve role → model string using the chooser (if present) or the
    /// static routing map as a fallback.
    async fn resolve_model(&self, role: &str) -> error::Result<String> {
        self.select_with_fallbacks(role, &[]).await
    }

    /// Resolve role → model string using the chooser (if present) or the
    /// static routing map as a fallback. If the chooser or static map
    /// don't have a match, try the `fallbacks` in order.
    pub async fn select_with_fallbacks(
        &self,
        role: &str,
        fallbacks: &[String],
    ) -> error::Result<String> {
        if let Some(ref chooser) = self.chooser {
            let budget = self.budget.lock().await;
            let remaining = budget.dollars_remaining();
            let max_budget = budget.max_dollars;
            drop(budget);

            if let Some(model) = chooser.select(role, remaining, max_budget) {
                return Ok(model.to_string());
            }
            // Fall through to static map if chooser has no match.
        }
        if let Some(model) = self.routing.get(role) {
            return Ok(model.clone());
        }
        for fallback in fallbacks {
            if let Some(model) = self.routing.get(fallback) {
                return Ok(model.clone());
            }
        }
        Err(error::Error::Config(format!(
            "no model configured for role `{role}` or fallbacks"
        )))
    }

    /// Make a completion call for the given agent role.
    ///
    /// Resolves role → model → provider, checks budget, calls the API,
    /// records usage, and retries on transient failures.
    #[tracing::instrument(skip(self, messages, response_format), fields(request_id))]
    pub async fn complete(
        &self,
        role: &str,
        messages: &[Message],
        temperature: f32,
        max_tokens: u32,
        response_format: Option<&serde_json::Value>,
    ) -> error::Result<RouterResponse> {
        // Generate unique request ID
        let request_id = Uuid::new_v4().to_string();
        tracing::Span::current().record("request_id", &request_id);

        // Check budget before calling.
        {
            let budget = self.budget.lock().await;
            budget.check()?;
        }

        // Resolve role → model string (chooser-aware).
        let model_string = self.resolve_model(role).await?;

        let (provider_name, model_id) = parse_model_string(&model_string);

        // Look up provider.
        let provider = self.providers.get(provider_name).ok_or_else(|| {
            error::Error::Config(format!(
                "provider `{provider_name}` not available (missing API key?)"
            ))
        })?;

        info!(role, model = %model_string, request_id = %request_id, "router: calling LLM");

        // Call with retry (up to 5 attempts on transient/overload errors).
        let mut last_err = None;
        for attempt in 1..=5u64 {
            match provider
                .complete(model_id, messages, temperature, max_tokens, response_format)
                .await
            {
                Ok(mut response) => {
                    // Set the request_id in the response
                    response.request_id = request_id.clone();

                    // Record usage.
                    let mut budget = self.budget.lock().await;
                    budget.record(
                        response.prompt_tokens,
                        response.completion_tokens,
                        response.cost,
                    );

                    info!(
                        role,
                        model = %response.model,
                        request_id = %request_id,
                        tokens = response.total_tokens,
                        cost = format!("${:.4}", response.cost),
                        latency_ms = response.latency_ms,
                        stop_reason = response.stop_reason.as_deref().unwrap_or("none"),
                        "router: LLM call complete"
                    );

                    return Ok(response);
                }
                Err(ProviderError::RateLimited { retry_after_ms }) => {
                    let wait = retry_after_ms.unwrap_or(1000 * attempt);
                    warn!(
                        role,
                        attempt,
                        request_id = %request_id,
                        wait_ms = wait,
                        "router: rate limited / overloaded, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                    last_err = Some(format!("rate limited on attempt {attempt}"));
                }
                Err(ProviderError::Http(e)) if attempt < 5 => {
                    let wait = 1000 * attempt;
                    warn!(
                        role,
                        attempt,
                        request_id = %request_id,
                        error = %e,
                        "router: transient HTTP error, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                    last_err = Some(e.to_string());
                }
                Err(e) => {
                    return Err(error::Error::Agent {
                        agent: role.into(),
                        reason: e.to_string(),
                    });
                }
            }
        }

        Err(error::Error::Agent {
            agent: role.into(),
            reason: format!(
                "exhausted retries: {}",
                last_err.unwrap_or_else(|| "unknown".into())
            ),
        })
    }

    /// Make a completion call with tool definitions for the given agent role.
    ///
    /// Same as `complete()` — resolves role, checks budget, retries — but
    /// passes tool definitions through to the provider.
    #[tracing::instrument(skip(self, messages, tools, response_format), fields(request_id))]
    pub async fn complete_with_tools(
        &self,
        role: &str,
        messages: &[Message],
        temperature: f32,
        max_tokens: u32,
        tools: &[ToolDefinition],
        response_format: Option<&serde_json::Value>,
    ) -> error::Result<RouterResponse> {
        // Generate unique request ID
        let request_id = Uuid::new_v4().to_string();
        tracing::Span::current().record("request_id", &request_id);

        // Check budget before calling.
        {
            let budget = self.budget.lock().await;
            budget.check()?;
        }

        // Resolve role → model string (chooser-aware).
        let model_string = self.resolve_model(role).await?;

        let (provider_name, model_id) = parse_model_string(&model_string);

        // Look up provider.
        let provider = self.providers.get(provider_name).ok_or_else(|| {
            error::Error::Config(format!(
                "provider `{provider_name}` not available (missing API key?)"
            ))
        })?;

        info!(
            role,
            model = %model_string,
            request_id = %request_id,
            tool_count = tools.len(),
            "router: calling LLM with tools"
        );

        // Call with retry (up to 5 attempts on transient/overload errors).
        let mut last_err = None;
        for attempt in 1..=5u64 {
            match provider
                .complete_with_tools(
                    model_id,
                    messages,
                    temperature,
                    max_tokens,
                    tools,
                    response_format,
                )
                .await
            {
                Ok(mut response) => {
                    // Set the request_id in the response
                    response.request_id = request_id.clone();

                    // Record usage.
                    let mut budget = self.budget.lock().await;
                    budget.record(
                        response.prompt_tokens,
                        response.completion_tokens,
                        response.cost,
                    );

                    info!(
                        role,
                        model = %response.model,
                        request_id = %request_id,
                        tokens = response.total_tokens,
                        cost = format!("${:.4}", response.cost),
                        latency_ms = response.latency_ms,
                        tool_calls = response.tool_calls.len(),
                        "router: LLM call with tools complete"
                    );

                    return Ok(response);
                }
                Err(ProviderError::RateLimited { retry_after_ms }) => {
                    let wait = retry_after_ms.unwrap_or(1000 * attempt);
                    warn!(
                        role,
                        attempt,
                        request_id = %request_id,
                        wait_ms = wait,
                        "router: rate limited / overloaded, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                    last_err = Some(format!("rate limited on attempt {attempt}"));
                }
                Err(ProviderError::Http(e)) if attempt < 5 => {
                    let wait = 1000 * attempt;
                    warn!(
                        role,
                        attempt,
                        request_id = %request_id,
                        error = %e,
                        "router: transient HTTP error, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                    last_err = Some(e.to_string());
                }
                Err(e) => {
                    return Err(error::Error::Agent {
                        agent: role.into(),
                        reason: e.to_string(),
                    });
                }
            }
        }

        Err(error::Error::Agent {
            agent: role.into(),
            reason: format!(
                "exhausted retries: {}",
                last_err.unwrap_or_else(|| "unknown".into())
            ),
        })
    }

    /// Pre-flight check: verify that all configured roles have available
    /// providers. Returns a list of `(role, error_message)` for any role
    /// whose provider is missing (e.g. API key not set).
    ///
    /// Call this before starting the pipeline to catch configuration errors
    /// early instead of failing deep in a multi-stage run.
    pub fn preflight_check(&self) -> Vec<(String, String)> {
        let mut errors = Vec::new();
        for (role, model_string) in &self.routing {
            let (provider_name, _model_id) = parse_model_string(model_string);
            if !self.providers.contains_key(provider_name) {
                errors.push((
                    role.clone(),
                    format!(
                        "provider `{provider_name}` not available for role `{role}` \
                         (model: {model_string}). Set the appropriate API key."
                    ),
                ));
            }
        }
        errors.sort_by(|a, b| a.0.cmp(&b.0));
        errors
    }

    /// Get a snapshot of the current budget state.
    pub async fn budget_summary(&self) -> glitchlab_kernel::budget::BudgetSummary {
        self.budget.lock().await.summary()
    }

    /// Check if budget is exceeded.
    pub async fn budget_exceeded(&self) -> bool {
        self.budget.lock().await.exceeded()
    }
}

// ---------------------------------------------------------------------------
// Provider factory
// ---------------------------------------------------------------------------

/// Create a provider instance from a `ProviderInit`.
///
/// Matches on `init.kind`:
/// - `"anthropic"` → `AnthropicProvider`
/// - `"gemini"` → `GeminiProvider`
/// - `"openai"` or anything else → `OpenAiProvider` (OpenAI-compatible)
///
/// For OpenAI-compatible providers, if `base_url` ends with `/v1`,
/// `/chat/completions` is appended automatically.
fn create_provider(init: &ProviderInit) -> Box<dyn Provider> {
    let name = init.name.clone().unwrap_or_else(|| init.kind.clone());

    match init.kind.as_str() {
        "anthropic" => match &init.base_url {
            Some(url) => Box::new(AnthropicProvider::with_base_url(
                init.api_key.clone(),
                url.clone(),
            )),
            None => Box::new(AnthropicProvider::new(init.api_key.clone())),
        },
        "gemini" => match &init.base_url {
            Some(url) => Box::new(GeminiProvider::with_base_url(
                init.api_key.clone(),
                url.clone(),
            )),
            None => Box::new(GeminiProvider::new(init.api_key.clone())),
        },
        _ => {
            // OpenAI-compatible (including "openai" and custom providers).
            let base_url = match &init.base_url {
                Some(url) => normalize_openai_url(url),
                None => "https://api.openai.com/v1/chat/completions".into(),
            };
            Box::new(OpenAiProvider::new(init.api_key.clone(), base_url, name))
        }
    }
}

/// If a URL ends with `/v1`, append `/chat/completions` for convenience.
fn normalize_openai_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        format!("{trimmed}/chat/completions")
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Provider, ProviderError, ProviderFuture};
    use crate::response::RouterResponse;
    use glitchlab_kernel::agent::{Message, MessageContent, MessageRole};
    use glitchlab_kernel::budget::BudgetTracker;

    struct MockProvider {
        response: RouterResponse,
    }

    impl MockProvider {
        fn ok() -> Self {
            Self {
                response: RouterResponse {
                    request_id: String::new(), // Will be set by router
                    content: r#"{"result": "ok"}"#.into(),
                    model: "mock/test-model".into(),
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    cost: 0.001,
                    latency_ms: 42,
                    tool_calls: vec![],
                    stop_reason: None,
                },
            }
        }
    }

    impl Provider for MockProvider {
        fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _temperature: f32,
            _max_tokens: u32,
            _response_format: Option<&serde_json::Value>,
        ) -> ProviderFuture<'_> {
            let resp = self.response.clone();
            Box::pin(async move { Ok(resp) })
        }
    }

    struct ErrorProvider;

    impl Provider for ErrorProvider {
        fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _temperature: f32,
            _max_tokens: u32,
            _response_format: Option<&serde_json::Value>,
        ) -> ProviderFuture<'_> {
            Box::pin(async move { Err(ProviderError::Parse("test error".into())) })
        }
    }

    fn test_messages() -> Vec<Message> {
        vec![
            Message {
                role: MessageRole::System,
                content: MessageContent::Text("You are a test.".into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("Hello".into()),
            },
        ]
    }

    #[tokio::test]
    async fn complete_with_mock_provider() {
        let routing = HashMap::from([("planner".to_string(), "mock/test".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let mut router = Router::new(routing, budget);
        router.register_provider("mock".into(), Arc::new(MockProvider::ok()));

        let result = router
            .complete("planner", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.content, r#"{"result": "ok"}"#);
        assert_eq!(response.total_tokens, 150);
    }

    #[tokio::test]
    async fn complete_tracks_budget() {
        let routing = HashMap::from([("planner".to_string(), "mock/test".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let mut router = Router::new(routing, budget);
        router.register_provider("mock".into(), Arc::new(MockProvider::ok()));

        router
            .complete("planner", &test_messages(), 0.2, 4096, None)
            .await
            .unwrap();

        let summary = router.budget_summary().await;
        assert_eq!(summary.total_tokens, 150);
        assert!((summary.estimated_cost - 0.001).abs() < f64::EPSILON);
        assert!(!router.budget_exceeded().await);
    }

    #[tokio::test]
    async fn complete_unknown_role_errors() {
        let routing = HashMap::new();
        let budget = BudgetTracker::new(100_000, 10.0);
        let router = Router::new(routing, budget);

        let result = router
            .complete("nonexistent", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn complete_missing_provider_errors() {
        let routing = HashMap::from([("planner".to_string(), "missing/test".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let router = Router::new(routing, budget);

        let result = router
            .complete("planner", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn complete_non_retryable_error() {
        let routing = HashMap::from([("planner".to_string(), "err/test".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let mut router = Router::new(routing, budget);
        router.register_provider("err".into(), Arc::new(ErrorProvider));

        let result = router
            .complete("planner", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn register_custom_provider() {
        let routing = HashMap::from([("custom".to_string(), "local/model".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let mut router = Router::new(routing, budget);
        router.register_provider("local".into(), Arc::new(MockProvider::ok()));

        let result = router
            .complete("custom", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(result.is_ok());
    }

    struct RateLimitedProvider;

    impl Provider for RateLimitedProvider {
        fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _temperature: f32,
            _max_tokens: u32,
            _response_format: Option<&serde_json::Value>,
        ) -> ProviderFuture<'_> {
            Box::pin(async move {
                Err(ProviderError::RateLimited {
                    retry_after_ms: Some(1),
                })
            })
        }
    }

    #[tokio::test]
    async fn complete_retries_on_rate_limit_then_exhausts() {
        let routing = HashMap::from([("planner".to_string(), "rl/test".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let mut router = Router::new(routing, budget);
        router.register_provider("rl".into(), Arc::new(RateLimitedProvider));

        let result = router
            .complete("planner", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("exhausted retries"), "got: {err_msg}");
    }

    #[test]
    fn preflight_check_all_providers_present() {
        let routing = HashMap::from([("planner".to_string(), "mock/test".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let mut router = Router::new(routing, budget);
        router.register_provider("mock".into(), Arc::new(MockProvider::ok()));
        let errors = router.preflight_check();
        assert!(errors.is_empty());
    }

    #[test]
    fn preflight_check_missing_provider() {
        let routing = HashMap::from([
            ("planner".to_string(), "anthropic/claude-sonnet".to_string()),
            ("debugger".to_string(), "openai/gpt-4".to_string()),
        ]);
        let budget = BudgetTracker::new(100_000, 10.0);
        // No providers registered (new() may init from env, but not "anthropic"/"openai" in test)
        let mut router = Router::new(HashMap::new(), budget);
        router.routing = routing;
        router.providers.clear();

        let errors = router.preflight_check();
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn preflight_check_reports_correct_provider_names() {
        let routing = HashMap::from([(
            "implementer".to_string(),
            "gemini/gemini-2.5-flash".to_string(),
        )]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let mut router = Router::new(HashMap::new(), budget);
        router.routing = routing;
        router.providers.clear();

        let errors = router.preflight_check();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "implementer");
        assert!(errors[0].1.contains("gemini"));
    }

    #[tokio::test]
    async fn budget_exceeded_blocks_call() {
        let routing = HashMap::from([("planner".to_string(), "mock/test".to_string())]);
        let budget = BudgetTracker::new(1, 0.0001);
        let mut router = Router::new(routing, budget);
        router.register_provider("mock".into(), Arc::new(MockProvider::ok()));

        // First call succeeds and uses up budget.
        router
            .complete("planner", &test_messages(), 0.2, 4096, None)
            .await
            .unwrap();

        // Second call should fail due to budget.
        let result = router
            .complete("planner", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn request_id_is_unique_and_valid_uuid() {
        let routing = HashMap::from([("planner".to_string(), "mock/test".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let mut router = Router::new(routing, budget);
        router.register_provider("mock".into(), Arc::new(MockProvider::ok()));

        // Make multiple requests
        let response1 = router
            .complete("planner", &test_messages(), 0.2, 4096, None)
            .await
            .unwrap();
        let response2 = router
            .complete("planner", &test_messages(), 0.2, 4096, None)
            .await
            .unwrap();

        // Check that request_ids are non-empty
        assert!(!response1.request_id.is_empty());
        assert!(!response2.request_id.is_empty());

        // Check that request_ids are unique
        assert_ne!(response1.request_id, response2.request_id);

        // Check that request_ids are valid UUIDs
        assert!(uuid::Uuid::parse_str(&response1.request_id).is_ok());
        assert!(uuid::Uuid::parse_str(&response2.request_id).is_ok());
    }

    #[tokio::test]
    async fn request_id_in_tools_response() {
        let routing = HashMap::from([("planner".to_string(), "mock/test".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let mut router = Router::new(routing, budget);
        router.register_provider("mock".into(), Arc::new(MockToolProvider::with_tool_call()));

        let response = router
            .complete_with_tools(
                "planner",
                &test_messages(),
                0.2,
                4096,
                &test_tool_defs(),
                None,
            )
            .await
            .unwrap();

        // Check that request_id is present and valid
        assert!(!response.request_id.is_empty());
        assert!(uuid::Uuid::parse_str(&response.request_id).is_ok());
    }

    fn test_tool_defs() -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "read_file".into(),
            description: "Read a file from disk".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        }]
    }

    struct MockToolProvider {
        response: RouterResponse,
    }

    impl MockToolProvider {
        fn with_tool_call() -> Self {
            Self {
                response: RouterResponse {
                    request_id: String::new(), // Will be set by router
                    content: String::new(),
                    model: "mock/test-model".into(),
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    cost: 0.001,
                    latency_ms: 42,
                    tool_calls: vec![glitchlab_kernel::tool::ToolCall {
                        id: "call_1".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path": "src/lib.rs"}),
                    }],
                    stop_reason: Some("tool_use".into()),
                },
            }
        }
    }

    impl Provider for MockToolProvider {
        fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _temperature: f32,
            _max_tokens: u32,
            _response_format: Option<&serde_json::Value>,
        ) -> ProviderFuture<'_> {
            let resp = self.response.clone();
            Box::pin(async move { Ok(resp) })
        }

        fn complete_with_tools(
            &self,
            _model: &str,
            _messages: &[Message],
            _temperature: f32,
            _max_tokens: u32,
            _tools: &[ToolDefinition],
            _response_format: Option<&serde_json::Value>,
        ) -> ProviderFuture<'_> {
            let resp = self.response.clone();
            Box::pin(async move { Ok(resp) })
        }
    }

    #[tokio::test]
    async fn complete_with_tools_with_mock_provider() {
        let routing = HashMap::from([("planner".to_string(), "mock/test".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let mut router = Router::new(routing, budget);
        router.register_provider("mock".into(), Arc::new(MockToolProvider::with_tool_call()));

        let result = router
            .complete_with_tools(
                "planner",
                &test_messages(),
                0.2,
                4096,
                &test_tool_defs(),
                None,
            )
            .await;
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(response.stop_reason.as_deref(), Some("tool_use"));
    }

    #[tokio::test]
    async fn complete_with_tools_tracks_budget() {
        let routing = HashMap::from([("planner".to_string(), "mock/test".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let mut router = Router::new(routing, budget);
        router.register_provider("mock".into(), Arc::new(MockToolProvider::with_tool_call()));

        router
            .complete_with_tools(
                "planner",
                &test_messages(),
                0.2,
                4096,
                &test_tool_defs(),
                None,
            )
            .await
            .unwrap();

        let summary = router.budget_summary().await;
        assert_eq!(summary.total_tokens, 150);
        assert!((summary.estimated_cost - 0.001).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn complete_with_tools_unknown_role_errors() {
        let routing = HashMap::new();
        let budget = BudgetTracker::new(100_000, 10.0);
        let router = Router::new(routing, budget);

        let result = router
            .complete_with_tools(
                "nonexistent",
                &test_messages(),
                0.2,
                4096,
                &test_tool_defs(),
                None,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn complete_with_tools_missing_provider_errors() {
        let routing = HashMap::from([("planner".to_string(), "missing/test".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let router = Router::new(routing, budget);

        let result = router
            .complete_with_tools(
                "planner",
                &test_messages(),
                0.2,
                4096,
                &test_tool_defs(),
                None,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn complete_with_tools_retries_on_rate_limit() {
        let routing = HashMap::from([("planner".to_string(), "rl/test".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let mut router = Router::new(routing, budget);
        router.register_provider("rl".into(), Arc::new(RateLimitedProvider));

        let result = router
            .complete_with_tools(
                "planner",
                &test_messages(),
                0.2,
                4096,
                &test_tool_defs(),
                None,
            )
            .await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("exhausted retries"), "got: {err_msg}");
    }

    #[tokio::test]
    async fn complete_with_tools_non_retryable_error() {
        let routing = HashMap::from([("planner".to_string(), "err/test".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let mut router = Router::new(routing, budget);
        router.register_provider("err".into(), Arc::new(ErrorProvider));

        let result = router
            .complete_with_tools(
                "planner",
                &test_messages(),
                0.2,
                4096,
                &test_tool_defs(),
                None,
            )
            .await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // ModelChooser integration tests
    // -----------------------------------------------------------------------

    use crate::chooser::{ModelChooser, ModelProfile, ModelTier, RolePreference};
    use std::collections::HashSet;

    fn test_chooser() -> ModelChooser {
        let models = vec![
            ModelProfile {
                model_string: "mock/cheap".into(),
                input_cost_per_m: 0.1,
                output_cost_per_m: 0.3,
                tier: ModelTier::Economy,
                capabilities: HashSet::from(["tool_use".into(), "code".into()]),
            },
            ModelProfile {
                model_string: "mock/mid".into(),
                input_cost_per_m: 0.5,
                output_cost_per_m: 1.0,
                tier: ModelTier::Standard,
                capabilities: HashSet::from(["tool_use".into(), "code".into()]),
            },
        ];
        let roles = HashMap::from([(
            "planner".into(),
            RolePreference {
                min_tier: ModelTier::Standard,
                required_capabilities: HashSet::new(),
            },
        )]);
        ModelChooser::new(models, roles, 0.7)
    }

    #[tokio::test]
    async fn resolve_model_uses_chooser_when_present() {
        let routing = HashMap::from([("planner".to_string(), "mock/fallback".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let router = Router::new(routing, budget).with_chooser(test_chooser());

        let model = router.resolve_model("planner").await.unwrap();
        // Chooser should pick "mock/mid" (Standard tier, cheapest eligible).
        assert_eq!(model, "mock/mid");
    }

    #[tokio::test]
    async fn resolve_model_falls_back_to_static() {
        let routing = HashMap::from([("planner".to_string(), "mock/fallback".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        // No chooser attached → uses static routing map.
        let router = Router::new(routing, budget);

        let model = router.resolve_model("planner").await.unwrap();
        assert_eq!(model, "mock/fallback");
    }

    #[tokio::test]
    async fn resolve_model_chooser_no_match_falls_to_static() {
        // Chooser with impossible requirements.
        let models = vec![ModelProfile {
            model_string: "mock/cheap".into(),
            input_cost_per_m: 0.1,
            output_cost_per_m: 0.3,
            tier: ModelTier::Economy,
            capabilities: HashSet::new(),
        }];
        let roles = HashMap::from([(
            "planner".into(),
            RolePreference {
                min_tier: ModelTier::Economy,
                required_capabilities: HashSet::from(["nonexistent".into()]),
            },
        )]);
        let chooser = ModelChooser::new(models, roles, 0.5);

        let routing = HashMap::from([("planner".to_string(), "mock/static".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let router = Router::new(routing, budget).with_chooser(chooser);

        let model = router.resolve_model("planner").await.unwrap();
        assert_eq!(model, "mock/static");
    }

    #[tokio::test]
    async fn complete_with_chooser() {
        let routing = HashMap::from([("planner".to_string(), "mock/fallback".to_string())]);
        let budget = BudgetTracker::new(100_000, 10.0);

        // The chooser will select "mock/mid", which starts with provider "mock".
        let mut router = Router::new(routing, budget).with_chooser(test_chooser());
        router.register_provider("mock".into(), Arc::new(MockProvider::ok()));

        let result = router
            .complete("planner", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // with_providers / create_provider tests
    // -----------------------------------------------------------------------

    use crate::provider::ProviderInit;

    #[test]
    fn with_providers_creates_provider() {
        let init = ProviderInit {
            kind: "anthropic".into(),
            api_key: "test-key".into(),
            base_url: None,
            name: None,
        };
        let inits = HashMap::from([("anthropic".into(), init)]);
        let routing = HashMap::from([("planner".into(), "anthropic/claude-test".into())]);
        let budget = BudgetTracker::new(100_000, 10.0);

        let router = Router::with_providers(routing, budget, inits);
        // The anthropic provider should be registered.
        assert!(router.providers.contains_key("anthropic"));
    }

    #[test]
    fn with_providers_env_fallback() {
        // Providers not in init_configs still try from_env().
        let routing = HashMap::new();
        let budget = BudgetTracker::new(100_000, 10.0);
        let router = Router::with_providers(routing, budget, HashMap::new());

        // We can't guarantee env vars are set, but the function should not panic.
        // Equivalent to Router::new().
        let _ = router.preflight_check();
    }

    #[test]
    fn with_providers_config_overrides_env() {
        // When config provides "anthropic", env fallback should NOT override it.
        let init = ProviderInit {
            kind: "anthropic".into(),
            api_key: "from-config".into(),
            base_url: Some("http://config-host".into()),
            name: None,
        };
        let inits = HashMap::from([("anthropic".into(), init)]);
        let routing = HashMap::new();
        let budget = BudgetTracker::new(100_000, 10.0);

        let router = Router::with_providers(routing, budget, inits);
        // anthropic should be present (from config, not env).
        assert!(router.providers.contains_key("anthropic"));
    }

    #[test]
    fn new_delegates_to_with_providers() {
        // Router::new() should behave the same as with_providers(empty).
        let routing = HashMap::from([("planner".into(), "mock/test".into())]);
        let budget = BudgetTracker::new(100_000, 10.0);
        let r1 = Router::new(routing.clone(), budget.clone());
        let r2 = Router::with_providers(routing, budget, HashMap::new());

        // Both should have the same provider set (env-based).
        assert_eq!(r1.providers.len(), r2.providers.len());
        for key in r1.providers.keys() {
            assert!(r2.providers.contains_key(key));
        }
    }

    #[test]
    fn create_provider_anthropic() {
        let init = ProviderInit {
            kind: "anthropic".into(),
            api_key: "key".into(),
            base_url: None,
            name: None,
        };
        let _provider = super::create_provider(&init);
    }

    #[test]
    fn create_provider_anthropic_with_base_url() {
        let init = ProviderInit {
            kind: "anthropic".into(),
            api_key: "key".into(),
            base_url: Some("http://localhost:8080".into()),
            name: None,
        };
        let _provider = super::create_provider(&init);
    }

    #[test]
    fn create_provider_gemini() {
        let init = ProviderInit {
            kind: "gemini".into(),
            api_key: "key".into(),
            base_url: None,
            name: None,
        };
        let _provider = super::create_provider(&init);
    }

    #[test]
    fn create_provider_gemini_with_base_url() {
        let init = ProviderInit {
            kind: "gemini".into(),
            api_key: "key".into(),
            base_url: Some("http://localhost:9090".into()),
            name: None,
        };
        let _provider = super::create_provider(&init);
    }

    #[test]
    fn create_provider_openai() {
        let init = ProviderInit {
            kind: "openai".into(),
            api_key: "key".into(),
            base_url: None,
            name: None,
        };
        let _provider = super::create_provider(&init);
    }

    #[test]
    fn create_provider_unknown_kind_uses_openai_compat() {
        let init = ProviderInit {
            kind: "ollama".into(),
            api_key: "none".into(),
            base_url: Some("http://localhost:11434/v1".into()),
            name: Some("ollama".into()),
        };
        let _provider = super::create_provider(&init);
    }

    #[test]
    fn normalize_openai_url_appends_chat_completions() {
        assert_eq!(
            super::normalize_openai_url("http://localhost:11434/v1"),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn normalize_openai_url_leaves_full_path() {
        assert_eq!(
            super::normalize_openai_url("http://localhost:11434/v1/chat/completions"),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn normalize_openai_url_trims_trailing_slash() {
        assert_eq!(
            super::normalize_openai_url("http://localhost:11434/v1/"),
            "http://localhost:11434/v1/chat/completions"
        );
    }
}
