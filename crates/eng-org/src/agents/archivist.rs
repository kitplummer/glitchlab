use glitchlab_kernel::agent::{
    Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageContent, MessageRole,
};
use glitchlab_kernel::error;

use super::build_user_message;
use super::json_response_format;
use super::parse::parse_json_response;
use crate::agents::RouterRef;

const SYSTEM_PROMPT: &str = r#"You are Archivist Nova, the documentation engine inside GLITCHLAB.

You are invoked AFTER successful implementation. Capture what was done and why.
Produce documentation artifacts.

Output schema (valid JSON only, no markdown, no commentary):
{
  "adr": {
    "title": "<ADR-NNN: Short descriptive title>",
    "status": "accepted",
    "context": "<what situation or problem prompted this change>",
    "decision": "<what was decided and implemented>",
    "consequences": "<what this means going forward>",
    "alternatives_considered": ["<alternative 1>", "<alternative 2>"]
  },
  "doc_updates": [
    {
      "file": "<path/to/doc.md>",
      "action": "create|append|update",
      "content": "<the documentation content>",
      "description": "<what this doc update covers>"
    }
  ],
  "architecture_notes": "<brief note about architectural implications>",
  "should_write_adr": <bool>
}

Rules:
- Only recommend an ADR for meaningful architectural decisions, not trivial changes.
- Keep documentation concise and actionable.
- Use the existing project's documentation style if visible in context.
- Produce valid JSON only."#;

pub struct ArchivistAgent {
    router: RouterRef,
}

impl ArchivistAgent {
    pub fn new(router: RouterRef) -> Self {
        Self { router }
    }
}

impl Agent for ArchivistAgent {
    fn role(&self) -> &str {
        "archivist"
    }

    fn persona(&self) -> &str {
        "Nova"
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

        let json_fmt = json_response_format();
        let response = self
            .router
            .complete("archivist", &messages, 0.2, 4096, Some(&json_fmt))
            .await?;

        let metadata = AgentMetadata {
            agent: "archivist".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        let fallback = serde_json::json!({
            "adr": null,
            "doc_updates": [],
            "architecture_notes": "Failed to parse archivist output",
            "should_write_adr": false
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
        let agent = ArchivistAgent::new(mock_router_ref());
        assert_eq!(agent.role(), "archivist");
        assert_eq!(agent.persona(), "Nova");
    }

    #[tokio::test]
    async fn execute_returns_output() {
        let agent = ArchivistAgent::new(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "archivist");
    }

    #[tokio::test]
    async fn execute_with_previous_output() {
        let agent = ArchivistAgent::new(mock_router_ref());
        let mut ctx = test_agent_context();
        ctx.previous_output = serde_json::json!({"plan": {}, "implementation": {}});
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "archivist");
    }
}
