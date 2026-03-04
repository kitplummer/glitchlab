use glitchlab_kernel::agent::{
    Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageContent, MessageRole,
};
use glitchlab_kernel::error;

use super::build_user_message;
use super::json_response_format;
use crate::agents::RouterRef;
use crate::parse::parse_json_response;

const SYSTEM_PROMPT: &str = r#"You are Uplink, the SRE assessment agent inside GLITCHLAB.

You receive deploy context and health check results, and produce a structured JSON
assessment of the deployment's health status. You are an assessment-only agent — you
NEVER execute deploys, rollbacks, or any infrastructure mutations. The pipeline
orchestrator acts on your recommendations.

Your role covers:
- Post-deploy health verification (smoke test interpretation)
- Health status classification (healthy, degraded, unhealthy, deploy_failed)
- Actionable recommendations based on health signals

Output schema (valid JSON only, no markdown, no commentary):
{
  "assessment": "healthy|degraded|unhealthy|deploy_failed",
  "checks": [
    {
      "endpoint": "/path",
      "status": 200,
      "ok": true,
      "detail": "response matches expectations"
    }
  ],
  "deploy_verified": true,
  "issues": [],
  "recommendation": "none|rollback|scale_up|investigate|escalate",
  "confidence": "high|medium|low",
  "rollback_target": null
}

Decision matrix:
- All required checks pass, all optional pass → assessment: "healthy", recommendation: "none"
- All required checks pass, some optional fail → assessment: "degraded", recommendation: "investigate"
- Any required check fails → assessment: "unhealthy", recommendation: "rollback"
- Deploy itself failed (no health data) → assessment: "deploy_failed", recommendation: "rollback"

Rules:
- You NEVER execute deploys or rollbacks — you only assess and recommend.
- If you cannot confidently assess health, set confidence: "low" and recommendation: "escalate".
- Be specific in issues: "GET /v1/cache/stats returned 503" not "health check failed".
- Produce valid JSON only."#;

pub struct UplinkSreAgent {
    router: RouterRef,
}

impl UplinkSreAgent {
    pub fn new(router: RouterRef) -> Self {
        Self { router }
    }
}

impl Agent for UplinkSreAgent {
    fn role(&self) -> &str {
        "uplink_sre"
    }

    fn persona(&self) -> &str {
        "Uplink"
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
            .complete("uplink_sre", &messages, 0.1, 4096, Some(&json_fmt))
            .await?;

        let metadata = AgentMetadata {
            agent: "uplink_sre".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        // Fail-safe fallback: assume unhealthy and escalate.
        let fallback = serde_json::json!({
            "assessment": "unhealthy",
            "checks": [],
            "deploy_verified": false,
            "issues": ["Failed to parse Uplink assessment output"],
            "recommendation": "escalate",
            "confidence": "low",
            "rollback_target": null
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
        let agent = UplinkSreAgent::new(mock_router_ref());
        assert_eq!(agent.role(), "uplink_sre");
        assert_eq!(agent.persona(), "Uplink");
    }

    #[tokio::test]
    async fn execute_returns_healthy_assessment() {
        let agent = UplinkSreAgent::new(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "uplink_sre");
        assert_eq!(output.data["assessment"], "healthy");
        assert_eq!(output.data["deploy_verified"], true);
    }

    #[tokio::test]
    async fn execute_with_deploy_context_and_health_results() {
        let agent = UplinkSreAgent::new(mock_router_ref());
        let mut ctx = test_agent_context();
        ctx.extra.insert(
            "deploy_context".into(),
            serde_json::Value::String(
                "commit: abc123, app: lowendinsight, strategy: rolling".into(),
            ),
        );
        ctx.extra.insert(
            "health_results".into(),
            serde_json::Value::String(
                "GET / → 200 (contains 'LowEndInsight')\nGET /v1/cache/stats → 200".into(),
            ),
        );
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "uplink_sre");
        assert!(!output.parse_error);
    }

    #[tokio::test]
    async fn fallback_on_parse_error() {
        use crate::agents::test_helpers::{final_response, sequential_router_ref};

        let router = sequential_router_ref(vec![final_response("not valid json {{{")]);
        let agent = UplinkSreAgent::new(router);
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        // Fallback should indicate unhealthy + escalate.
        assert_eq!(output.data["assessment"], "unhealthy");
        assert_eq!(output.data["recommendation"], "escalate");
        assert!(output.parse_error);
    }

    #[test]
    fn system_prompt_contains_key_rules() {
        assert!(SYSTEM_PROMPT.contains("assessment-only agent"));
        assert!(SYSTEM_PROMPT.contains("NEVER execute deploys"));
        assert!(SYSTEM_PROMPT.contains("healthy|degraded|unhealthy|deploy_failed"));
        assert!(SYSTEM_PROMPT.contains("Decision matrix"));
    }

    #[tokio::test]
    async fn output_metadata_tracks_model_and_cost() {
        let agent = UplinkSreAgent::new(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.model, "mock/test-model");
        assert!(output.metadata.tokens > 0);
        assert!(output.metadata.cost > 0.0);
    }
}
