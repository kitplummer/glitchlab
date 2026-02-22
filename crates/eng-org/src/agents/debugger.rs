use glitchlab_kernel::agent::{
    Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageRole,
};
use glitchlab_kernel::error;

use super::build_user_message;
use super::parse::parse_json_response;
use crate::agents::RouterRef;

const SYSTEM_PROMPT: &str = r#"You are Reroute, the debug engine inside GLITCHLAB.

You are invoked ONLY when tests fail or builds break.
Produce a MINIMAL fix. Nothing more. Fix the EXACT failure.
Do not refactor, add features, or clean up code.

Output schema (valid JSON only, no markdown, no commentary):
{
  "diagnosis": "<what went wrong and why>",
  "root_cause": "<the specific root cause>",
  "fix": {
    "changes": [
      {
        "file": "<path/to/file>",
        "action": "modify",
        "patch": "<unified diff of the fix>",
        "description": "<what this fixes>"
      }
    ]
  },
  "confidence": "high|medium|low",
  "should_retry": <bool>,
  "notes": "<any additional context>"
}

Rules:
- Fix ONLY what is broken. Minimal diff.
- If you cannot diagnose the issue, set confidence to "low" and should_retry to false.
- If a previous fix attempt was provided in context, do NOT repeat it.
- Produce valid JSON only."#;

pub struct DebuggerAgent {
    router: RouterRef,
}

impl DebuggerAgent {
    pub fn new(router: RouterRef) -> Self {
        Self { router }
    }
}

impl Agent for DebuggerAgent {
    fn role(&self) -> &str {
        "debugger"
    }

    fn persona(&self) -> &str {
        "Reroute"
    }

    async fn execute(&self, ctx: &AgentContext) -> error::Result<AgentOutput> {
        let messages = vec![
            Message {
                role: MessageRole::System,
                content: SYSTEM_PROMPT.into(),
            },
            Message {
                role: MessageRole::User,
                content: build_user_message(ctx),
            },
        ];

        let response = self
            .router
            .complete("debugger", &messages, 0.2, 4096, None)
            .await?;

        let metadata = AgentMetadata {
            agent: "debugger".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        let fallback = serde_json::json!({
            "diagnosis": "Failed to parse debugger output",
            "root_cause": "unknown",
            "fix": { "changes": [] },
            "confidence": "low",
            "should_retry": false,
            "notes": null
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
        let agent = DebuggerAgent::new(mock_router_ref());
        assert_eq!(agent.role(), "debugger");
        assert_eq!(agent.persona(), "Reroute");
    }

    #[tokio::test]
    async fn execute_returns_output() {
        let agent = DebuggerAgent::new(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "debugger");
    }

    #[tokio::test]
    async fn execute_with_previous_output() {
        let agent = DebuggerAgent::new(mock_router_ref());
        let mut ctx = test_agent_context();
        ctx.previous_output = serde_json::json!({"test_output": "FAILED: assertion error"});
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "debugger");
    }
}
