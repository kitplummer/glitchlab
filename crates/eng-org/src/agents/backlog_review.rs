use glitchlab_kernel::agent::{
    Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageContent, MessageRole,
};
use glitchlab_kernel::error;

use super::json_response_format;
use super::parse::parse_json_response;
use crate::agents::RouterRef;

// ---------------------------------------------------------------------------
// BacklogReviewAgent
// ---------------------------------------------------------------------------

const REVIEW_SYSTEM_PROMPT: &str = r#"You are Curator, the backlog review agent inside GLITCHLAB.

Your job is to review the full set of open beads (tasks) against the project's
Architecture Decision Records (ADRs) and the current codebase state. You run
BEFORE the batch task loop as a pre-sweep to keep the backlog healthy.

You check:

1. Is the bead's work already done? (code already exists, feature already implemented)
2. Is the bead still aligned with its ADR label? (ADR may have been superseded)
3. Is the bead stale? (no progress, superseded by later work)
4. Should the bead's priority be adjusted based on ADR phase ordering?

You receive:
- A list of open beads with their IDs, objectives, priorities, and statuses
- ADR summaries extracted from the repository

Output schema (valid JSON only, no markdown, no commentary):
{
  "summary": "<one-line overview of review findings>",
  "total_beads_reviewed": 0,
  "actions": [
    {
      "bead_id": "<bead ID>",
      "action": "close | reprioritize | flag_stale | flag_misaligned | no_action",
      "reason": "<why this action>",
      "new_priority": null,
      "confidence": 0.0
    }
  ],
  "adr_coverage": [
    {
      "adr_file": "<filename>",
      "open_beads_count": 0,
      "completion_estimate": "complete | partial | not_started",
      "notes": "<observations>"
    }
  ],
  "recommendations": ["<actionable suggestions>"]
}

Rules:
- Review ALL beads provided. Every bead must have an entry in "actions".
- "close" means the bead's work already exists in the codebase.
- "reprioritize" means the priority should change (provide new_priority 0-100).
- "flag_stale" means the bead has been superseded or is no longer relevant.
- "flag_misaligned" means the bead doesn't match its ADR requirements.
- "no_action" means the bead is fine as-is.
- Set confidence 0.0-1.0 for each action. Higher = more certain.
- Produce valid JSON only."#;

pub struct BacklogReviewAgent {
    router: RouterRef,
}

impl BacklogReviewAgent {
    pub fn new(router: RouterRef) -> Self {
        Self { router }
    }

    /// Build the user message for backlog review from beads + ADR context.
    pub fn build_prompt(beads_json: &str, adr_context: &str) -> String {
        let mut msg = String::new();
        msg.push_str("## Open Beads (Tasks)\n\n");
        msg.push_str(beads_json);
        if !adr_context.is_empty() {
            msg.push_str("\n\n");
            msg.push_str(adr_context);
        }
        msg
    }
}

impl Agent for BacklogReviewAgent {
    fn role(&self) -> &str {
        "backlog_review"
    }

    fn persona(&self) -> &str {
        "Curator"
    }

    async fn execute(&self, ctx: &AgentContext) -> error::Result<AgentOutput> {
        let messages = vec![
            Message {
                role: MessageRole::System,
                content: MessageContent::Text(REVIEW_SYSTEM_PROMPT.into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text(ctx.objective.clone()),
            },
        ];

        let json_fmt = json_response_format();
        let response = self
            .router
            .complete("backlog_review", &messages, 0.2, 8192, Some(&json_fmt))
            .await?;

        let metadata = AgentMetadata {
            agent: "backlog_review".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        let fallback = serde_json::json!({
            "summary": "parse_error",
            "total_beads_reviewed": 0,
            "actions": [],
            "adr_coverage": [],
            "recommendations": []
        });

        Ok(parse_json_response(&response.content, metadata, fallback))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::test_helpers::{mock_router_ref, test_agent_context};
    use glitchlab_kernel::agent::Agent;

    fn review_router_ref() -> RouterRef {
        use crate::agents::test_helpers::MockProvider;
        use glitchlab_kernel::budget::BudgetTracker;
        use std::collections::HashMap;
        use std::sync::Arc;

        let routing = HashMap::from([
            ("backlog_review".to_string(), "mock/test".to_string()),
            ("planner".to_string(), "mock/test".to_string()),
            ("implementer".to_string(), "mock/test".to_string()),
            ("debugger".to_string(), "mock/test".to_string()),
            ("security".to_string(), "mock/test".to_string()),
            ("release".to_string(), "mock/test".to_string()),
            ("archivist".to_string(), "mock/test".to_string()),
            ("architect_triage".to_string(), "mock/test".to_string()),
            ("architect_review".to_string(), "mock/test".to_string()),
            ("ops_diagnosis".to_string(), "mock/test".to_string()),
            ("ciso".to_string(), "mock/test".to_string()),
        ]);
        let budget = BudgetTracker::new(1_000_000, 100.0);
        let mut router = glitchlab_router::Router::new(routing, budget);
        router.register_provider("mock".into(), Arc::new(MockProvider));
        Arc::new(router)
    }

    #[test]
    fn role_and_persona() {
        let agent = BacklogReviewAgent::new(mock_router_ref());
        assert_eq!(agent.role(), "backlog_review");
        assert_eq!(agent.persona(), "Curator");
    }

    #[tokio::test]
    async fn execute_returns_output() {
        let agent = BacklogReviewAgent::new(review_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "backlog_review");
    }

    #[tokio::test]
    async fn fallback_on_parse_error() {
        use crate::agents::test_helpers::{SequentialMockProvider, final_response};
        use glitchlab_kernel::budget::BudgetTracker;
        use std::collections::HashMap;
        use std::sync::Arc;

        let routing = HashMap::from([
            ("backlog_review".to_string(), "seq/test".to_string()),
            ("planner".to_string(), "seq/test".to_string()),
            ("implementer".to_string(), "seq/test".to_string()),
            ("debugger".to_string(), "seq/test".to_string()),
            ("security".to_string(), "seq/test".to_string()),
            ("release".to_string(), "seq/test".to_string()),
            ("archivist".to_string(), "seq/test".to_string()),
            ("architect_triage".to_string(), "seq/test".to_string()),
            ("architect_review".to_string(), "seq/test".to_string()),
            ("ops_diagnosis".to_string(), "seq/test".to_string()),
            ("ciso".to_string(), "seq/test".to_string()),
        ]);
        let budget = BudgetTracker::new(1_000_000, 100.0);
        let mut router = glitchlab_router::Router::new(routing, budget);
        router.register_provider(
            "seq".into(),
            Arc::new(SequentialMockProvider::new(vec![final_response(
                "This is not JSON at all.",
            )])),
        );
        let router_ref = Arc::new(router);

        let agent = BacklogReviewAgent::new(router_ref);
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert!(output.parse_error);
        assert_eq!(output.data["summary"], "parse_error");
        assert_eq!(output.data["total_beads_reviewed"], 0);
    }

    #[test]
    fn system_prompt_contains_schema() {
        assert!(REVIEW_SYSTEM_PROMPT.contains("total_beads_reviewed"));
        assert!(REVIEW_SYSTEM_PROMPT.contains("actions"));
        assert!(REVIEW_SYSTEM_PROMPT.contains("adr_coverage"));
        assert!(REVIEW_SYSTEM_PROMPT.contains("recommendations"));
        assert!(REVIEW_SYSTEM_PROMPT.contains("confidence"));
    }

    #[test]
    fn build_prompt_includes_beads_and_adrs() {
        let beads = r#"[{"id": "gl-1e0.1", "objective": "Do stuff"}]"#;
        let adrs = "## Architecture Decision Records\n\n### adr-001.md — Use Rust\n";
        let prompt = BacklogReviewAgent::build_prompt(beads, adrs);
        assert!(prompt.contains("gl-1e0.1"));
        assert!(prompt.contains("Open Beads"));
        assert!(prompt.contains("Architecture Decision Records"));
        assert!(prompt.contains("adr-001.md"));
    }

    #[test]
    fn build_prompt_no_adrs() {
        let beads = r#"[{"id": "gl-1"}]"#;
        let prompt = BacklogReviewAgent::build_prompt(beads, "");
        assert!(prompt.contains("gl-1"));
        assert!(!prompt.contains("Architecture Decision Records"));
    }
}
