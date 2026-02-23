use glitchlab_kernel::agent::{
    Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageContent, MessageRole,
};
use glitchlab_kernel::error;

use super::build_user_message;
use super::json_response_format;
use super::parse::parse_json_response;
use crate::agents::RouterRef;

const SYSTEM_PROMPT: &str = r#"You are Semver Sam, the release guardian inside GLITCHLAB.

Analyze code changes and determine the semantic versioning impact.

Output schema (valid JSON only, no markdown, no commentary):
{
  "version_bump": "none|patch|minor|major",
  "reasoning": "<why this bump level>",
  "changelog_entry": "<markdown changelog entry>",
  "breaking_changes": [],
  "migration_notes": "<any migration needed, or null>",
  "risk_summary": "<brief risk assessment for release>",
  "tests_verified": <bool>
}

Bump rules:
- patch: bug fixes, internal refactors, no API change
- minor: new features, non-breaking additions
- major: breaking changes to public API
- none: docs only, comments, formatting

Test verification:
- Check if tests_added is non-empty in the implementation output.
- If no tests were added for a non-trivial change, flag this in risk_summary.
- Set tests_verified to true if the implementation includes tests, false otherwise.

Rules:
- Be conservative — prefer patch over minor, minor over major.
- Only flag "major" if there are actual breaking changes.
- Produce valid JSON only."#;

pub struct ReleaseAgent {
    router: RouterRef,
}

impl ReleaseAgent {
    pub fn new(router: RouterRef) -> Self {
        Self { router }
    }
}

impl Agent for ReleaseAgent {
    fn role(&self) -> &str {
        "release"
    }

    fn persona(&self) -> &str {
        "Semver Sam"
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
            .complete("release", &messages, 0.2, 2048, Some(&json_fmt))
            .await?;

        let metadata = AgentMetadata {
            agent: "release".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        let fallback = serde_json::json!({
            "version_bump": "none",
            "reasoning": "Failed to parse release output",
            "changelog_entry": "",
            "breaking_changes": [],
            "migration_notes": null,
            "risk_summary": "unknown",
            "tests_verified": false
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
        let agent = ReleaseAgent::new(mock_router_ref());
        assert_eq!(agent.role(), "release");
        assert_eq!(agent.persona(), "Semver Sam");
    }

    #[tokio::test]
    async fn execute_returns_output() {
        let agent = ReleaseAgent::new(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "release");
    }

    #[tokio::test]
    async fn execute_with_previous_output() {
        let agent = ReleaseAgent::new(mock_router_ref());
        let mut ctx = test_agent_context();
        ctx.previous_output = serde_json::json!({"diff": "+new line", "plan": {}});
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "release");
    }

    #[test]
    fn system_prompt_mentions_tests_verified() {
        assert!(SYSTEM_PROMPT.contains("tests_verified"));
        assert!(SYSTEM_PROMPT.contains("tests_added"));
    }

    #[test]
    fn release_fallback_has_tests_verified() {
        // Verify the fallback JSON includes tests_verified: false.
        let fallback = serde_json::json!({
            "version_bump": "none",
            "reasoning": "Failed to parse release output",
            "changelog_entry": "",
            "breaking_changes": [],
            "migration_notes": null,
            "risk_summary": "unknown",
            "tests_verified": false
        });
        assert_eq!(fallback["tests_verified"], false);
    }
}
