use glitchlab_kernel::agent::{
    Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageContent, MessageRole,
};
use glitchlab_kernel::error;

use super::build_user_message;
use super::json_response_format;
use super::parse::parse_json_response;
use crate::agents::RouterRef;

const SYSTEM_PROMPT: &str = r#"You are Circuit, the ops-diagnosis agent inside GLITCHLAB.

You receive TQM-detected anti-patterns (stuck agents, decomposition loops, boundary
violations, etc.) together with run context, and produce a structured diagnosis with
specific, actionable remediation tasks for the normal pipeline to execute.

Output schema (valid JSON only, no markdown, no commentary):
{
  "diagnosis": "<concise description of what is going wrong>",
  "root_cause": "<why it is happening>",
  "remediation_tasks": [
    {
      "objective": "<specific, actionable fix objective>",
      "target_files": ["<path/to/file>"],
      "estimated_complexity": "trivial|small|medium",
      "rationale": "<why this fix will resolve the issue>"
    }
  ],
  "scope_assessment": {
    "within_scope": true,
    "modifies_kernel": false
  },
  "suggested_priority": 2,
  "confidence": "high|medium|low",
  "escalate": false
}

Rules:
- NEVER suggest changes to crates/kernel. If the fix requires kernel changes, set
  scope_assessment.modifies_kernel = true and escalate = true.
- Each remediation task should touch at most 2 files.
- If you cannot confidently diagnose the issue, set escalate = true and leave
  remediation_tasks empty.
- Be specific: "Fix the boundary check in orchestrator.rs line 450" not "investigate the issue".
- Produce valid JSON only."#;

pub struct OpsDiagnosisAgent {
    router: RouterRef,
}

impl OpsDiagnosisAgent {
    pub fn new(router: RouterRef) -> Self {
        Self { router }
    }
}

impl Agent for OpsDiagnosisAgent {
    fn role(&self) -> &str {
        "ops_diagnosis"
    }

    fn persona(&self) -> &str {
        "Circuit"
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
            .complete("ops_diagnosis", &messages, 0.2, 4096, Some(&json_fmt))
            .await?;

        let metadata = AgentMetadata {
            agent: "ops_diagnosis".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        let fallback = serde_json::json!({
            "diagnosis": "Failed to parse Circuit output",
            "root_cause": "unknown",
            "remediation_tasks": [],
            "scope_assessment": { "within_scope": false, "modifies_kernel": false },
            "suggested_priority": 5,
            "confidence": "low",
            "escalate": true
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
        let agent = OpsDiagnosisAgent::new(mock_router_ref());
        assert_eq!(agent.role(), "ops_diagnosis");
        assert_eq!(agent.persona(), "Circuit");
    }

    #[tokio::test]
    async fn execute_returns_output() {
        let agent = OpsDiagnosisAgent::new(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "ops_diagnosis");
    }

    #[tokio::test]
    async fn execute_with_tqm_context() {
        let agent = OpsDiagnosisAgent::new(mock_router_ref());
        let mut ctx = test_agent_context();
        ctx.previous_output = serde_json::json!({
            "patterns": [
                {
                    "kind": "stuck_agents",
                    "severity": "warning",
                    "occurrences": 3,
                    "affected_tasks": ["t1"]
                }
            ]
        });
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "ops_diagnosis");
    }

    #[tokio::test]
    async fn fallback_on_parse_error() {
        use crate::agents::test_helpers::{final_response, sequential_router_ref};

        // Return invalid JSON to trigger fallback.
        let router = sequential_router_ref(vec![final_response("not valid json {{{")]);
        let agent = OpsDiagnosisAgent::new(router);
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        // Fallback should have escalate: true.
        assert_eq!(output.data["escalate"], true);
        assert!(
            output.data["remediation_tasks"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
}
