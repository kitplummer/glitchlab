pub mod uplink;

use std::sync::Arc;

use glitchlab_kernel::agent::AgentContext;

/// Shared reference to the router, used by all ops agents.
pub type RouterRef = Arc<glitchlab_router::Router>;

/// Response format hint for JSON-only agents.
/// Providers that support structured output use this to enable JSON mode.
pub(crate) fn json_response_format() -> serde_json::Value {
    serde_json::json!({"type": "json_object"})
}

/// Assemble the user message for an ops agent from its context.
///
/// Includes:
/// - Objective text
/// - Deploy context (from `extra["deploy_context"]`)
/// - Health check results (from `extra["health_results"]`)
/// - Constraints
/// - Previous stage output
pub(crate) fn build_user_message(ctx: &AgentContext) -> String {
    let mut msg = ctx.objective.clone();

    // Deploy context — injected by the ops pipeline for deploy verification.
    if let Some(serde_json::Value::String(dc)) = ctx.extra.get("deploy_context")
        && !dc.is_empty()
    {
        msg.push_str("\n\n## Deploy Context\n\n");
        msg.push_str(dc);
    }

    // Health check results — injected after running smoke tests.
    if let Some(serde_json::Value::String(hr)) = ctx.extra.get("health_results")
        && !hr.is_empty()
    {
        msg.push_str("\n\n## Health Check Results\n\n");
        msg.push_str(hr);
    }

    // Constraints.
    if !ctx.constraints.is_empty() {
        msg.push_str("\n\n## Constraints\n");
        for constraint in &ctx.constraints {
            msg.push_str(&format!("\n- {constraint}"));
        }
    }

    // Previous stage output.
    if !ctx.previous_output.is_null() {
        msg.push_str("\n\n## Previous Stage Output\n\n");
        msg.push_str(
            &serde_json::to_string_pretty(&ctx.previous_output)
                .unwrap_or_else(|_| ctx.previous_output.to_string()),
        );
    }

    msg
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;
    use glitchlab_kernel::agent::{AgentContext, Message};
    use glitchlab_kernel::budget::BudgetTracker;
    use glitchlab_router::RouterResponse;
    use glitchlab_router::provider::{Provider, ProviderFuture};
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    pub(crate) struct MockProvider;

    impl Provider for MockProvider {
        fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _temperature: f32,
            _max_tokens: u32,
            _response_format: Option<&serde_json::Value>,
        ) -> ProviderFuture<'_> {
            Box::pin(async move {
                Ok(RouterResponse {
                    request_id: String::new(),
                    content: r#"{"assessment": "healthy", "checks": [], "deploy_verified": true, "issues": [], "recommendation": "none", "confidence": "high", "rollback_target": null}"#.into(),
                    model: "mock/test-model".into(),
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    cost: 0.001,
                    latency_ms: 42,
                    tool_calls: vec![],
                    stop_reason: None,
                })
            })
        }

        fn complete_with_tools(
            &self,
            model: &str,
            messages: &[Message],
            temperature: f32,
            max_tokens: u32,
            _tools: &[glitchlab_kernel::tool::ToolDefinition],
            response_format: Option<&serde_json::Value>,
        ) -> ProviderFuture<'_> {
            self.complete(model, messages, temperature, max_tokens, response_format)
        }
    }

    /// Mock provider that returns pre-scripted responses in sequence.
    pub(crate) struct SequentialMockProvider {
        responses: Mutex<VecDeque<RouterResponse>>,
    }

    impl SequentialMockProvider {
        pub(crate) fn new(responses: Vec<RouterResponse>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }
    }

    impl Provider for SequentialMockProvider {
        fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _temperature: f32,
            _max_tokens: u32,
            _response_format: Option<&serde_json::Value>,
        ) -> ProviderFuture<'_> {
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| RouterResponse {
                    request_id: String::new(),
                    content: "no more responses".into(),
                    model: "mock/test".into(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    cost: 0.0,
                    latency_ms: 0,
                    tool_calls: vec![],
                    stop_reason: None,
                });
            Box::pin(async move { Ok(response) })
        }

        fn complete_with_tools(
            &self,
            model: &str,
            messages: &[Message],
            temperature: f32,
            max_tokens: u32,
            _tools: &[glitchlab_kernel::tool::ToolDefinition],
            response_format: Option<&serde_json::Value>,
        ) -> ProviderFuture<'_> {
            self.complete(model, messages, temperature, max_tokens, response_format)
        }
    }

    /// Build a RouterRef backed by a SequentialMockProvider.
    pub(crate) fn sequential_router_ref(responses: Vec<RouterResponse>) -> RouterRef {
        let routing = HashMap::from([("uplink_sre".to_string(), "seq/test".to_string())]);
        let budget = BudgetTracker::new(1_000_000, 100.0);
        let mut router = glitchlab_router::Router::new(routing, budget);
        router.register_provider(
            "seq".into(),
            Arc::new(SequentialMockProvider::new(responses)),
        );
        Arc::new(router)
    }

    /// Create a simple RouterResponse with no tool calls.
    pub(crate) fn final_response(content: &str) -> RouterResponse {
        RouterResponse {
            request_id: String::new(),
            content: content.into(),
            model: "mock/test".into(),
            prompt_tokens: 50,
            completion_tokens: 25,
            total_tokens: 75,
            cost: 0.0005,
            latency_ms: 20,
            tool_calls: vec![],
            stop_reason: Some("end_turn".into()),
        }
    }

    pub(crate) fn mock_router_ref() -> RouterRef {
        let routing = HashMap::from([("uplink_sre".to_string(), "mock/test".to_string())]);
        let budget = BudgetTracker::new(1_000_000, 100.0);
        let mut router = glitchlab_router::Router::new(routing, budget);
        router.register_provider("mock".into(), Arc::new(MockProvider));
        Arc::new(router)
    }

    pub(crate) fn test_agent_context() -> AgentContext {
        AgentContext {
            task_id: "test-1".into(),
            objective: "Verify deploy health for lowendinsight".into(),
            repo_path: "/tmp/repo".into(),
            working_dir: "/tmp/repo/worktree".into(),
            constraints: vec![],
            acceptance_criteria: vec![],
            risk_level: "low".into(),
            file_context: HashMap::new(),
            previous_output: serde_json::Value::Null,
            extra: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn base_ctx() -> AgentContext {
        AgentContext {
            task_id: "t1".into(),
            objective: "Assess deploy health".into(),
            repo_path: "/repo".into(),
            working_dir: "/repo/wt".into(),
            constraints: vec![],
            acceptance_criteria: vec![],
            risk_level: "low".into(),
            file_context: HashMap::new(),
            previous_output: serde_json::Value::Null,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn build_user_message_objective_only() {
        let ctx = base_ctx();
        let msg = build_user_message(&ctx);
        assert_eq!(msg, "Assess deploy health");
    }

    #[test]
    fn build_user_message_with_deploy_context() {
        let mut ctx = base_ctx();
        ctx.extra.insert(
            "deploy_context".into(),
            serde_json::Value::String("commit abc123, app lowendinsight".into()),
        );
        let msg = build_user_message(&ctx);
        assert!(msg.contains("## Deploy Context"));
        assert!(msg.contains("commit abc123"));
    }

    #[test]
    fn build_user_message_with_health_results() {
        let mut ctx = base_ctx();
        ctx.extra.insert(
            "health_results".into(),
            serde_json::Value::String("GET / → 200 OK".into()),
        );
        let msg = build_user_message(&ctx);
        assert!(msg.contains("## Health Check Results"));
        assert!(msg.contains("GET / → 200 OK"));
    }

    #[test]
    fn build_user_message_with_constraints() {
        let mut ctx = base_ctx();
        ctx.constraints = vec!["No rollback".into(), "Budget: $0.50".into()];
        let msg = build_user_message(&ctx);
        assert!(msg.contains("## Constraints"));
        assert!(msg.contains("- No rollback"));
        assert!(msg.contains("- Budget: $0.50"));
    }

    #[test]
    fn build_user_message_with_previous_output() {
        let mut ctx = base_ctx();
        ctx.previous_output = serde_json::json!({"pre_check": "passed"});
        let msg = build_user_message(&ctx);
        assert!(msg.contains("## Previous Stage Output"));
        assert!(msg.contains("pre_check"));
    }

    #[test]
    fn build_user_message_all_populated() {
        let mut ctx = base_ctx();
        ctx.extra.insert(
            "deploy_context".into(),
            serde_json::Value::String("deploy info".into()),
        );
        ctx.extra.insert(
            "health_results".into(),
            serde_json::Value::String("all healthy".into()),
        );
        ctx.constraints = vec!["Stay safe".into()];
        ctx.previous_output = serde_json::json!({"result": "ok"});

        let msg = build_user_message(&ctx);
        assert!(msg.starts_with("Assess deploy health"));
        assert!(msg.contains("## Deploy Context"));
        assert!(msg.contains("## Health Check Results"));
        assert!(msg.contains("## Constraints"));
        assert!(msg.contains("## Previous Stage Output"));
    }

    #[test]
    fn json_response_format_is_json_object() {
        let fmt = json_response_format();
        assert_eq!(fmt["type"], "json_object");
    }
}
