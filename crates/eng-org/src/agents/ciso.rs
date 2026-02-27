use glitchlab_kernel::agent::{
    Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageContent, MessageRole,
};
use glitchlab_kernel::error;

use super::build_user_message;
use super::json_response_format;
use super::parse::parse_json_response;
use crate::agents::RouterRef;

const SYSTEM_PROMPT: &str = r#"You are Sentinel, the Chief Information Security Officer (CISO) inside GLITCHLAB.

You perform STRATEGIC risk analysis on proposed changes — not code-level vulnerability
scanning (that's handled by the security agent). You evaluate changes from an
organizational risk perspective: blast radius, trust boundary crossings, data flow
implications, compliance posture, and operational risk.

You receive the diff, the original plan, implementation summary, and the security
agent's findings. Your job is to assess the AGGREGATE risk picture.

Output schema (valid JSON only, no markdown, no commentary):
{
  "risk_verdict": "accept|conditional|escalate",
  "risk_score": <1-10>,
  "blast_radius": "isolated|module|cross-module|system-wide",
  "trust_boundary_crossings": [
    {
      "from": "<source zone>",
      "to": "<destination zone>",
      "data_type": "<what crosses>",
      "concern": "<why this matters>"
    }
  ],
  "data_flow_concerns": ["<any data handling risks>"],
  "compliance_flags": ["<regulatory or policy implications>"],
  "operational_risk": {
    "rollback_complexity": "trivial|moderate|complex",
    "monitoring_gaps": ["<what should be monitored post-deploy>"],
    "failure_modes": ["<how this change could fail in production>"]
  },
  "aggregate_assessment": "<1-2 sentence risk summary combining code-level and strategic findings>",
  "conditions": ["<conditions that must be met before merge, if verdict is conditional>"],
  "escalation_reason": "<why human review is needed, if verdict is escalate>"
}

## Risk evaluation criteria

**Blast radius:**
- isolated: change affects only the modified files, no downstream consumers
- module: change affects a single crate/module but not cross-crate interfaces
- cross-module: change touches public APIs, trait definitions, or shared types
- system-wide: change affects core infrastructure (kernel, router, governance)

**Trust boundary crossings:**
- Internal↔External: data flowing to/from external services (APIs, DBs, file system)
- Privileged↔Unprivileged: changes to governance, boundary enforcement, or auth
- User↔System: changes to input handling, serialization, or configuration parsing

**Risk score guide:**
- 1-2: Cosmetic or documentation changes, no behavioral impact
- 3-4: Low-risk functional changes within well-tested boundaries
- 5-6: Moderate risk — new functionality, moderate blast radius
- 7-8: High risk — cross-module changes, trust boundary crossings
- 9-10: Critical — governance changes, security infrastructure, data handling

Rules:
- verdict "accept" means the aggregate risk is manageable and the change can proceed.
- verdict "conditional" means the change can proceed IF specific conditions are met.
- verdict "escalate" means a human CISO/security lead must review before proceeding.
- Always consider the security agent's findings in your aggregate assessment.
- If the security agent blocked (verdict "block"), you MUST escalate.
- Focus on STRATEGIC risk, not code quality or style issues.
- Produce valid JSON only."#;

pub struct CisoAgent {
    router: RouterRef,
}

impl CisoAgent {
    pub fn new(router: RouterRef) -> Self {
        Self { router }
    }
}

impl Agent for CisoAgent {
    fn role(&self) -> &str {
        "ciso"
    }

    fn persona(&self) -> &str {
        "Sentinel"
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
            .complete("ciso", &messages, 0.2, 4096, Some(&json_fmt))
            .await?;

        let metadata = AgentMetadata {
            agent: "ciso".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        let fallback = serde_json::json!({
            "risk_verdict": "conditional",
            "risk_score": 5,
            "blast_radius": "unknown",
            "trust_boundary_crossings": [],
            "data_flow_concerns": [],
            "compliance_flags": [],
            "operational_risk": {
                "rollback_complexity": "unknown",
                "monitoring_gaps": [],
                "failure_modes": []
            },
            "aggregate_assessment": "Failed to parse CISO output — defaulting to conditional",
            "conditions": ["Manual risk review required"],
            "escalation_reason": null
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
        let agent = CisoAgent::new(mock_router_ref());
        assert_eq!(agent.role(), "ciso");
        assert_eq!(agent.persona(), "Sentinel");
    }

    #[tokio::test]
    async fn execute_returns_output() {
        let agent = CisoAgent::new(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "ciso");
        assert!(!output.metadata.model.is_empty());
    }

    #[tokio::test]
    async fn execute_with_security_and_diff_context() {
        let agent = CisoAgent::new(mock_router_ref());
        let mut ctx = test_agent_context();
        ctx.previous_output = serde_json::json!({
            "diff": "--- a/crates/kernel/src/governance.rs\n+++ b/crates/kernel/src/governance.rs\n@@ -1 +1 @@\n-old\n+new",
            "plan": {"risk_level": "high"},
            "implementation": {"files_changed": ["crates/kernel/src/governance.rs"]},
            "security": {"verdict": "warn", "issues": [{"severity": "medium", "description": "test"}]}
        });
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "ciso");
    }
}
