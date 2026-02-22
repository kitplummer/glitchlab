pub mod archivist;
pub mod debugger;
pub mod implementer;
pub mod planner;
pub mod release;
pub mod security;

mod parse;

use std::sync::Arc;

use glitchlab_kernel::agent::AgentContext;

/// Shared reference to the router, used by all agents.
pub type RouterRef = Arc<glitchlab_router::Router>;

/// Build the full user message from agent context, incorporating previous
/// stage output, file contents, and constraints alongside the objective.
///
/// This is used by all agents so they can see each other's output across
/// pipeline stages.
pub(crate) fn build_user_message(ctx: &AgentContext) -> String {
    let mut msg = ctx.objective.clone();

    // Previous stage output (the critical fix for agent context blindness).
    if !ctx.previous_output.is_null() {
        msg.push_str("\n\n## Previous Stage Output\n\n");
        // Pretty-print for readability in the LLM context window.
        msg.push_str(
            &serde_json::to_string_pretty(&ctx.previous_output)
                .unwrap_or_else(|_| ctx.previous_output.to_string()),
        );
    }

    // File contents relevant to the task.
    if !ctx.file_context.is_empty() {
        msg.push_str("\n\n## Relevant File Contents\n");
        let mut paths: Vec<_> = ctx.file_context.keys().collect();
        paths.sort();
        for path in paths {
            let content = &ctx.file_context[path];
            msg.push_str(&format!("\n### `{path}`\n\n```\n{content}\n```\n"));
        }
    }

    // Task constraints.
    if !ctx.constraints.is_empty() {
        msg.push_str("\n\n## Constraints\n\n");
        for constraint in &ctx.constraints {
            msg.push_str(&format!("- {constraint}\n"));
        }
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
    use std::collections::HashMap;

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
                    content: r#"{"result": "ok", "steps": [{"step_number": 1, "description": "add feature", "files": ["src/new.rs"], "action": "create"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "trivial", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false, "verdict": "pass", "issues": [], "version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": [], "diagnosis": "none", "root_cause": "none", "fix": {"changes": []}, "confidence": "high", "should_retry": false, "changes": [{"file": "src/new.rs", "action": "create", "content": "pub fn greet() -> &'static str { \"hello\" }\n", "description": "add greet function"}], "tests_added": [], "commit_message": "feat: add greet function", "summary": "test", "adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#.into(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn base_ctx() -> AgentContext {
        AgentContext {
            task_id: "t1".into(),
            objective: "Do the thing".into(),
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
        assert_eq!(msg, "Do the thing");
    }

    #[test]
    fn build_user_message_with_previous_output() {
        let mut ctx = base_ctx();
        ctx.previous_output = serde_json::json!({"plan": "step 1"});
        let msg = build_user_message(&ctx);
        assert!(msg.contains("## Previous Stage Output"));
        assert!(msg.contains("step 1"));
    }

    #[test]
    fn build_user_message_with_file_context() {
        let mut ctx = base_ctx();
        ctx.file_context
            .insert("src/lib.rs".into(), "fn hello() {}".into());
        let msg = build_user_message(&ctx);
        assert!(msg.contains("## Relevant File Contents"));
        assert!(msg.contains("`src/lib.rs`"));
        assert!(msg.contains("fn hello() {}"));
    }

    #[test]
    fn build_user_message_with_constraints() {
        let mut ctx = base_ctx();
        ctx.constraints = vec!["No new deps".into(), "Keep it simple".into()];
        let msg = build_user_message(&ctx);
        assert!(msg.contains("## Constraints"));
        assert!(msg.contains("- No new deps"));
        assert!(msg.contains("- Keep it simple"));
    }

    #[test]
    fn build_user_message_all_populated() {
        let mut ctx = base_ctx();
        ctx.previous_output = serde_json::json!({"result": "ok"});
        ctx.file_context
            .insert("Cargo.toml".into(), "[package]".into());
        ctx.constraints = vec!["Stay safe".into()];

        let msg = build_user_message(&ctx);
        assert!(msg.starts_with("Do the thing"));
        assert!(msg.contains("## Previous Stage Output"));
        assert!(msg.contains("## Relevant File Contents"));
        assert!(msg.contains("## Constraints"));
    }
}
