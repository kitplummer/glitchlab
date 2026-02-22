pub mod archivist;
pub mod debugger;
pub mod implementer;
pub mod planner;
pub mod release;
pub mod security;

mod parse;

use std::sync::Arc;

/// Shared reference to the router, used by all agents.
pub type RouterRef = Arc<glitchlab_router::Router>;

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;
    use std::collections::HashMap;
    use glitchlab_kernel::agent::{AgentContext, Message};
    use glitchlab_kernel::budget::BudgetTracker;
    use glitchlab_router::provider::{Provider, ProviderFuture};
    use glitchlab_router::RouterResponse;

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
                    content: r#"{"result": "ok", "steps": [], "verdict": "pass", "issues": [], "version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": [], "diagnosis": "none", "root_cause": "none", "fix": {"changes": []}, "confidence": "high", "should_retry": false, "changes": [], "tests_added": [], "commit_message": "test", "summary": "test", "adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#.into(),
                    model: "mock/test-model".into(),
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    cost: 0.001,
                    latency_ms: 42,
                })
            })
        }
    }

    pub(crate) fn mock_router_ref() -> RouterRef {
        let routing = HashMap::from([
            ("planner".to_string(), "mock/test".to_string()),
            ("implementer".to_string(), "mock/test".to_string()),
            ("debugger".to_string(), "mock/test".to_string()),
            ("security".to_string(), "mock/test".to_string()),
            ("release".to_string(), "mock/test".to_string()),
            ("archivist".to_string(), "mock/test".to_string()),
        ]);
        let budget = BudgetTracker::new(1_000_000, 100.0);
        let mut router = glitchlab_router::Router::new(routing, budget);
        router.register_provider("mock".into(), Arc::new(MockProvider));
        Arc::new(router)
    }

    pub(crate) fn test_agent_context() -> AgentContext {
        AgentContext {
            task_id: "test-1".into(),
            objective: "Fix the bug in auth module".into(),
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
