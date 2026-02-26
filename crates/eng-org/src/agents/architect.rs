use glitchlab_kernel::agent::{
    Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageContent, MessageRole,
};
use glitchlab_kernel::error;

use super::build_user_message;
use super::json_response_format;
use super::parse::parse_json_response;
use crate::agents::RouterRef;

// ---------------------------------------------------------------------------
// ArchitectTriageAgent
// ---------------------------------------------------------------------------

const TRIAGE_SYSTEM_PROMPT: &str = r#"You are Blueprint, the architect triage agent inside GLITCHLAB.

Your job is to evaluate whether a planned task is necessary and architecturally sound
BEFORE implementation begins. You check:

1. Is the work already done? (code already exists, feature already implemented)
2. Is the plan architecturally feasible within the codebase?
3. Is the scope appropriate — not too broad, not redundant?

You receive the planner's output and relevant file contents. Compare the plan
against existing code to detect duplication.

Output schema (valid JSON only, no markdown, no commentary):
{
  "verdict": "proceed|already_done|needs_refinement",
  "confidence": <0.0-1.0>,
  "reasoning": "<why this verdict>",
  "evidence": ["<specific files/functions that support the verdict>"],
  "architectural_notes": "<any architectural concerns or observations>",
  "suggested_changes": ["<optional suggestions if needs_refinement>"]
}

## Protected paths

Some paths in the repository are protected by project policy and CANNOT be modified by
the tool dispatcher. If the plan's `files_likely_affected` includes ANY protected path,
the implementation WILL fail. In this case, return verdict "needs_refinement" with
`suggested_changes` explaining that the protected paths must be excluded and the task
decomposed or declared infeasible.

Rules:
- verdict "already_done" means the planned work already exists in the codebase.
- verdict "needs_refinement" means the plan has issues but isn't fundamentally wrong.
- verdict "proceed" means the plan is sound and should be implemented.
- When uncertain, prefer "proceed" — don't block work unnecessarily.
- If the plan touches protected paths, ALWAYS return "needs_refinement".
- Produce valid JSON only."#;

pub struct ArchitectTriageAgent {
    router: RouterRef,
}

impl ArchitectTriageAgent {
    pub fn new(router: RouterRef) -> Self {
        Self { router }
    }
}

impl Agent for ArchitectTriageAgent {
    fn role(&self) -> &str {
        "architect_triage"
    }

    fn persona(&self) -> &str {
        "Blueprint"
    }

    async fn execute(&self, ctx: &AgentContext) -> error::Result<AgentOutput> {
        let messages = vec![
            Message {
                role: MessageRole::System,
                content: MessageContent::Text(TRIAGE_SYSTEM_PROMPT.into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text(build_user_message(ctx)),
            },
        ];

        let json_fmt = json_response_format();
        let response = self
            .router
            .complete("architect_triage", &messages, 0.2, 4096, Some(&json_fmt))
            .await?;

        let metadata = AgentMetadata {
            agent: "architect_triage".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        let fallback = serde_json::json!({
            "verdict": "proceed",
            "confidence": 0.0,
            "reasoning": "Failed to parse triage output — defaulting to proceed",
            "evidence": [],
            "architectural_notes": "",
            "suggested_changes": []
        });

        Ok(parse_json_response(&response.content, metadata, fallback))
    }
}

// ---------------------------------------------------------------------------
// ArchitectReviewAgent
// ---------------------------------------------------------------------------

const REVIEW_SYSTEM_PROMPT: &str = r#"You are Blueprint, the architect review agent inside GLITCHLAB.

Your job is to review completed changes AFTER implementation, BEFORE merge.
You evaluate diff quality, architectural fitness, and gate the merge decision.

You receive the full diff, the original plan, implementation summary, and
security review results.

Output schema (valid JSON only, no markdown, no commentary):
{
  "verdict": "merge|request_changes|close",
  "confidence": <0.0-1.0>,
  "reasoning": "<why this verdict>",
  "quality_score": <1-10>,
  "issues": [
    {
      "severity": "critical|high|medium|low",
      "description": "<what the issue is>",
      "suggestion": "<how to fix>"
    }
  ],
  "architectural_fitness": "<how well changes fit the architecture>",
  "merge_strategy": "squash|merge|rebase"
}

Rules:
- verdict "merge" means changes are ready — proceed to merge.
- verdict "request_changes" means fixable issues — stop and report.
- verdict "close" means fundamentally wrong — reject entirely.
- quality_score 1-3: poor, 4-6: acceptable, 7-9: good, 10: excellent.
- When uncertain, prefer "request_changes" — don't auto-merge bad code.
- Produce valid JSON only."#;

pub struct ArchitectReviewAgent {
    router: RouterRef,
}

impl ArchitectReviewAgent {
    pub fn new(router: RouterRef) -> Self {
        Self { router }
    }
}

impl Agent for ArchitectReviewAgent {
    fn role(&self) -> &str {
        "architect_review"
    }

    fn persona(&self) -> &str {
        "Blueprint"
    }

    async fn execute(&self, ctx: &AgentContext) -> error::Result<AgentOutput> {
        let messages = vec![
            Message {
                role: MessageRole::System,
                content: MessageContent::Text(REVIEW_SYSTEM_PROMPT.into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text(build_user_message(ctx)),
            },
        ];

        let json_fmt = json_response_format();
        let response = self
            .router
            .complete("architect_review", &messages, 0.2, 4096, Some(&json_fmt))
            .await?;

        let metadata = AgentMetadata {
            agent: "architect_review".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        let fallback = serde_json::json!({
            "verdict": "request_changes",
            "confidence": 0.0,
            "reasoning": "Failed to parse review output — defaulting to request_changes",
            "quality_score": 0,
            "issues": [],
            "architectural_fitness": "unknown",
            "merge_strategy": "squash"
        });

        Ok(parse_json_response(&response.content, metadata, fallback))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::test_helpers::{mock_router_ref, test_agent_context};
    use glitchlab_kernel::agent::Agent;

    // --- ArchitectTriageAgent tests ---

    #[test]
    fn triage_role_and_persona() {
        let agent = ArchitectTriageAgent::new(mock_router_ref());
        assert_eq!(agent.role(), "architect_triage");
        assert_eq!(agent.persona(), "Blueprint");
    }

    #[tokio::test]
    async fn triage_execute_returns_output() {
        let agent = ArchitectTriageAgent::new(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "architect_triage");
    }

    #[tokio::test]
    async fn triage_execute_with_previous_output() {
        let agent = ArchitectTriageAgent::new(mock_router_ref());
        let mut ctx = test_agent_context();
        ctx.previous_output = serde_json::json!({"steps": [{"description": "add feature"}]});
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "architect_triage");
    }

    // --- ArchitectReviewAgent tests ---

    #[test]
    fn review_role_and_persona() {
        let agent = ArchitectReviewAgent::new(mock_router_ref());
        assert_eq!(agent.role(), "architect_review");
        assert_eq!(agent.persona(), "Blueprint");
    }

    #[tokio::test]
    async fn review_execute_returns_output() {
        let agent = ArchitectReviewAgent::new(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "architect_review");
    }

    #[tokio::test]
    async fn review_execute_with_previous_output() {
        let agent = ArchitectReviewAgent::new(mock_router_ref());
        let mut ctx = test_agent_context();
        ctx.previous_output = serde_json::json!({
            "diff": "--- a/file\n+++ b/file",
            "plan": {},
            "implementation": {},
            "security": {"verdict": "pass"}
        });
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "architect_review");
    }
}
