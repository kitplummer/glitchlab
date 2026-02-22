use glitchlab_kernel::agent::{Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageRole};
use glitchlab_kernel::error;

use super::parse::parse_json_response;
use crate::agents::RouterRef;

const SYSTEM_PROMPT: &str = r#"You are Firewall Frankie, the security guard inside GLITCHLAB.

Review code changes BEFORE they become a PR. Look for security issues,
dangerous patterns, and policy violations.

Output schema (valid JSON only, no markdown, no commentary):
{
  "verdict": "pass|warn|block",
  "issues": [
    {
      "severity": "critical|high|medium|low|info",
      "file": "<path/to/file>",
      "line": <line number or null>,
      "description": "<what the issue is>",
      "recommendation": "<how to fix it>"
    }
  ],
  "dependency_changes": {
    "added": [],
    "removed": [],
    "risk_assessment": "none|low|medium|high"
  },
  "boundary_violations": [],
  "summary": "<brief security summary>"
}

Check for:
- Unsafe operations (unwrap in production paths, eval, exec, shell injection)
- Hardcoded secrets or credentials
- New dependencies (supply chain risk)
- Overly permissive file/network access
- Missing input validation
- Cryptographic misuse
- Changes to protected paths
- Unsafe deserialization
- SQL injection, XSS, command injection

Rules:
- verdict "block" means critical issues that MUST be fixed before merge.
- verdict "warn" means issues that should be reviewed but aren't blocking.
- verdict "pass" means no significant issues found.
- Produce valid JSON only."#;

pub struct SecurityAgent {
    router: RouterRef,
}

impl SecurityAgent {
    pub fn new(router: RouterRef) -> Self {
        Self { router }
    }
}

impl Agent for SecurityAgent {
    fn role(&self) -> &str {
        "security"
    }

    fn persona(&self) -> &str {
        "Firewall Frankie"
    }

    async fn execute(&self, ctx: &AgentContext) -> error::Result<AgentOutput> {
        let messages = vec![
            Message {
                role: MessageRole::System,
                content: SYSTEM_PROMPT.into(),
            },
            Message {
                role: MessageRole::User,
                content: ctx.objective.clone(),
            },
        ];

        let response = self.router.complete("security", &messages, 0.2, 4096, None).await?;

        let metadata = AgentMetadata {
            agent: "security".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        let fallback = serde_json::json!({
            "verdict": "warn",
            "issues": [],
            "dependency_changes": { "added": [], "removed": [], "risk_assessment": "unknown" },
            "boundary_violations": [],
            "summary": "Failed to parse security output"
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
        let agent = SecurityAgent::new(mock_router_ref());
        assert_eq!(agent.role(), "security");
        assert_eq!(agent.persona(), "Firewall Frankie");
    }

    #[tokio::test]
    async fn execute_returns_output() {
        let agent = SecurityAgent::new(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "security");
    }
}
