use glitchlab_kernel::agent::{
    Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageContent, MessageRole,
};
use glitchlab_kernel::error;

use super::build_user_message;
use super::parse::parse_json_response;
use crate::agents::RouterRef;

const SYSTEM_PROMPT: &str = r#"You are Patch, the implementation engine inside GLITCHLAB.

You receive a plan and produce code changes. Follow the plan exactly. No feature creep.
Keep diffs minimal. Always add/update tests. Use idiomatic patterns for the language.

Output schema (valid JSON only, no markdown, no commentary):
{
  "changes": [
    {
      "file": "<path/to/file>",
      "action": "modify|create|delete",
      "content": "<full file content if create, or null for delete>",
      "patch": "<unified diff for modify (preferred for existing files)>",
      "description": "<what this change does>"
    }
  ],
  "tests_added": [
    {
      "file": "<path/to/test_file>",
      "content": "<full test file content or additions>",
      "description": "<what this tests>"
    }
  ],
  "commit_message": "<conventional commit message>",
  "summary": "<brief human-readable summary of all changes>"
}

Rules:
- Prefer unified diffs (patch field) over full content for modifications.
- Always include tests for new functionality.
- Keep changes focused on the plan — do not refactor, clean up, or add features beyond scope.
- Produce valid JSON only."#;

pub struct ImplementerAgent {
    router: RouterRef,
}

impl ImplementerAgent {
    pub fn new(router: RouterRef) -> Self {
        Self { router }
    }
}

impl Agent for ImplementerAgent {
    fn role(&self) -> &str {
        "implementer"
    }

    fn persona(&self) -> &str {
        "Patch"
    }

    async fn execute(&self, ctx: &AgentContext) -> error::Result<AgentOutput> {
        let messages = vec![
            Message {
                role: MessageRole::System,
                content: MessageContent::Text(SYSTEM_PROMPT.into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text(build_user_message(ctx)),
            },
        ];

        let response = self
            .router
            .complete("implementer", &messages, 0.2, 16384, None)
            .await?;

        let metadata = AgentMetadata {
            agent: "implementer".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        let fallback = serde_json::json!({
            "changes": [],
            "tests_added": [],
            "commit_message": "chore: no changes produced",
            "summary": "Failed to parse implementer output"
        });

        Ok(parse_json_response(&response.content, metadata, fallback))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::test_helpers::{mock_router_ref, test_agent_context};
    use glitchlab_kernel::agent::Agent;

    #[test]
    fn role_and_persona() {
        let agent = ImplementerAgent::new(mock_router_ref());
        assert_eq!(agent.role(), "implementer");
        assert_eq!(agent.persona(), "Patch");
    }

    #[tokio::test]
    async fn execute_returns_output() {
        let agent = ImplementerAgent::new(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "implementer");
    }

    #[tokio::test]
    async fn execute_with_previous_output() {
        let agent = ImplementerAgent::new(mock_router_ref());
        let mut ctx = test_agent_context();
        ctx.previous_output = serde_json::json!({"steps": [{"description": "add feature"}]});
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "implementer");
    }
}
