use glitchlab_kernel::agent::{
    Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageContent, MessageRole,
};
use glitchlab_kernel::error;

use super::build_user_message;
use super::json_response_format;
use super::parse::parse_json_response;
use crate::agents::RouterRef;

const SYSTEM_PROMPT: &str = r#"You are Professor Zap, the planning engine inside GLITCHLAB.

Given a development task, produce a structured execution plan as valid JSON.
No markdown, no commentary — only the JSON object.

Output schema:
{
  "steps": [
    {
      "step_number": <int>,
      "description": "<what to do>",
      "files": ["<path/to/file>"],
      "action": "modify|create|delete"
    }
  ],
  "files_likely_affected": ["<path>"],
  "requires_core_change": <bool>,
  "risk_level": "low|medium|high",
  "risk_notes": "<why this risk level>",
  "test_strategy": ["<what tests to add or run>"],
  "estimated_complexity": "trivial|small|medium|large",
  "dependencies_affected": <bool>,
  "public_api_changed": <bool>
}

Rules:
- Keep steps minimal and atomic.
- List ALL files that will be touched.
- If the task is ambiguous, choose the simplest interpretation.
- If the task requires changes to protected paths, set requires_core_change to true.
- CRITICAL: Output ONLY the raw JSON object. No text before or after it.
  No markdown code fences. No explanations. Just the JSON."#;

pub struct PlannerAgent {
    router: RouterRef,
}

impl PlannerAgent {
    pub fn new(router: RouterRef) -> Self {
        Self { router }
    }
}

impl Agent for PlannerAgent {
    fn role(&self) -> &str {
        "planner"
    }

    fn persona(&self) -> &str {
        "Professor Zap"
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
            .complete("planner", &messages, 0.2, 16_384, Some(&json_fmt))
            .await?;

        let metadata = AgentMetadata {
            agent: "planner".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        let fallback = serde_json::json!({
            "steps": [],
            "files_likely_affected": [],
            "requires_core_change": false,
            "risk_level": "unknown",
            "risk_notes": "Failed to parse planner output",
            "test_strategy": [],
            "estimated_complexity": "unknown",
            "dependencies_affected": false,
            "public_api_changed": false
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
        let agent = PlannerAgent::new(mock_router_ref());
        assert_eq!(agent.role(), "planner");
        assert_eq!(agent.persona(), "Professor Zap");
    }

    #[tokio::test]
    async fn execute_returns_output() {
        let agent = PlannerAgent::new(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "planner");
        assert!(!output.metadata.model.is_empty());
    }

    #[tokio::test]
    async fn execute_with_previous_output() {
        let agent = PlannerAgent::new(mock_router_ref());
        let mut ctx = test_agent_context();
        ctx.previous_output = serde_json::json!({"prior": "data"});
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "planner");
    }
}
