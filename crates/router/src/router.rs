use std::collections::HashMap;
use std::sync::Arc;

use glitchlab_kernel::agent::Message;
use glitchlab_kernel::budget::BudgetTracker;
use glitchlab_kernel::error;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::provider::anthropic::AnthropicProvider;
use crate::provider::openai::OpenAiProvider;
use crate::provider::{Provider, ProviderError, parse_model_string};
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
}

impl Router {
    /// Build a router from a routing config and budget.
    ///
    /// Providers are lazily created from environment variables.
    /// Missing API keys are not an error until a model from that
    /// provider is actually requested.
    pub fn new(routing: HashMap<String, String>, budget: BudgetTracker) -> Self {
        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();

        // Try to initialize each provider from env.
        if let Ok(p) = AnthropicProvider::from_env() {
            providers.insert("anthropic".into(), Arc::new(p));
        }
        if let Ok(p) = OpenAiProvider::openai_from_env() {
            providers.insert("openai".into(), Arc::new(p));
        }
        if let Ok(p) = OpenAiProvider::gemini_from_env() {
            providers.insert("gemini".into(), Arc::new(p));
        }

        Self {
            routing,
            providers,
            budget: Arc::new(Mutex::new(budget)),
        }
    }

    /// Register a custom provider (e.g. Ollama, LM Studio).
    pub fn register_provider(&mut self, name: String, provider: Arc<dyn Provider>) {
        self.providers.insert(name, provider);
    }

    /// Make a completion call for the given agent role.
    ///
    /// Resolves role → model → provider, checks budget, calls the API,
    /// records usage, and retries on transient failures.
    pub async fn complete(
        &self,
        role: &str,
        messages: &[Message],
        temperature: f32,
        max_tokens: u32,
        response_format: Option<&serde_json::Value>,
    ) -> error::Result<RouterResponse> {
        // Check budget before calling.
        {
            let budget = self.budget.lock().await;
            budget.check()?;
        }

        // Resolve role → model string.
        let model_string = self.routing.get(role).ok_or_else(|| {
            error::Error::Config(format!("no model configured for role `{role}`"))
        })?;

        let (provider_name, model_id) = parse_model_string(model_string);

        // Look up provider.
        let provider = self.providers.get(provider_name).ok_or_else(|| {
            error::Error::Config(format!(
                "provider `{provider_name}` not available (missing API key?)"
            ))
        })?;

        info!(role, model = model_string, "router: calling LLM");

        // Call with retry (up to 3 attempts on transient errors).
        let mut last_err = None;
        for attempt in 1..=3 {
            match provider
                .complete(model_id, messages, temperature, max_tokens, response_format)
                .await
            {
                Ok(response) => {
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
                        tokens = response.total_tokens,
                        cost = format!("${:.4}", response.cost),
                        latency_ms = response.latency_ms,
                        "router: LLM call complete"
                    );

                    return Ok(response);
                }
                Err(ProviderError::RateLimited { retry_after_ms }) => {
                    let wait = retry_after_ms.unwrap_or(1000 * attempt);
                    warn!(
                        role,
                        attempt,
                        wait_ms = wait,
                        "router: rate limited, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
                    last_err = Some(format!("rate limited on attempt {attempt}"));
                }
                Err(ProviderError::Http(e)) if attempt < 3 => {
                    let wait = 1000 * attempt;
                    warn!(
                        role,
                        attempt,
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

    /// Get a snapshot of the current budget state.
    pub async fn budget_summary(&self) -> glitchlab_kernel::budget::BudgetSummary {
        self.budget.lock().await.summary()
    }

    /// Check if budget is exceeded.
    pub async fn budget_exceeded(&self) -> bool {
        self.budget.lock().await.exceeded()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Provider, ProviderError, ProviderFuture};
    use crate::response::RouterResponse;
    use glitchlab_kernel::agent::{Message, MessageRole};
    use glitchlab_kernel::budget::BudgetTracker;

    struct MockProvider {
        response: RouterResponse,
    }

    impl MockProvider {
        fn ok() -> Self {
            Self {
                response: RouterResponse {
                    content: r#"{"result": "ok"}"#.into(),
                    model: "mock/test-model".into(),
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    cost: 0.001,
                    latency_ms: 42,
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
                content: "You are a test.".into(),
            },
            Message {
                role: MessageRole::User,
                content: "Hello".into(),
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
}
