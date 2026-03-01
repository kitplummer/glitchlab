use glitchlab_kernel::agent::{
    Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageContent, MessageRole,
};
use glitchlab_kernel::error;

use super::json_response_format;
use super::parse::parse_json_response;
use crate::agents::RouterRef;

// ---------------------------------------------------------------------------
// AdrDecomposerAgent
// ---------------------------------------------------------------------------

const DECOMPOSER_SYSTEM_PROMPT: &str = r#"You are Cartographer, the ADR decomposer agent inside GLITCHLAB.

Your job is to read a single Architecture Decision Record (ADR) and decompose it
into actionable beads (tasks) that engineers or automated agents can execute.

You receive:
- The full text of an ADR
- A list of existing beads (for dedup — do NOT create beads for work that already exists)
- Optional consultation context from security and architect agents

Rules:
- Skip ADRs with status "Superseded" or "Rejected" — return empty beads array.
- Every bead must have a unique `id_suffix` that is descriptive and kebab-case.
- Label every bead with `adr:<adr_filename>` so the system can trace beads to their ADR.
- Each bead should be a single, concrete, implementable unit of work (S or M size).
- Do NOT create beads for work that already has matching beads in the existing list.
- Provide dependency ordering via `depends_on` referencing other id_suffixes.
- Be conservative — when unsure, note it in `implementation_notes` rather than creating a bead.

Output schema (valid JSON only, no markdown, no commentary):
{
  "adr_filename": "<filename of the ADR>",
  "adr_title": "<title extracted from the ADR>",
  "summary": "<one-line summary of what this ADR requires>",
  "beads": [
    {
      "id_suffix": "<kebab-case suffix, e.g. add-deploy-stage>",
      "title": "<short imperative title>",
      "description": "<what needs to be done>",
      "issue_type": "task | feature | chore",
      "priority": 0,
      "depends_on": ["<other id_suffix>"],
      "labels": ["adr:<filename>"],
      "estimated_size": "S | M | L | XL",
      "files_likely_affected": ["path/to/file.rs"]
    }
  ],
  "security_concerns": ["<any security implications noted>"],
  "implementation_notes": "<general notes for implementers>",
  "confidence": 0.0
}

Priority scale: 0 = critical, 1 = high, 2 = medium, 3 = low, 4 = backlog.
Confidence scale: 0.0 = very uncertain, 1.0 = fully confident in decomposition.
Produce valid JSON only."#;

pub struct AdrDecomposerAgent {
    router: RouterRef,
}

impl AdrDecomposerAgent {
    pub fn new(router: RouterRef) -> Self {
        Self { router }
    }

    /// Build the user message for ADR decomposition.
    ///
    /// `adr_content` — full text of the ADR file.
    /// `existing_beads_json` — JSON array of existing beads for dedup.
    /// `consultation_context` — optional output from security/architect consultation.
    pub fn build_prompt(
        adr_content: &str,
        existing_beads_json: &str,
        consultation_context: &str,
    ) -> String {
        let mut msg = String::new();
        msg.push_str("## ADR Content\n\n");
        msg.push_str(adr_content);
        msg.push_str("\n\n## Existing Beads (for dedup)\n\n");
        msg.push_str(existing_beads_json);
        if !consultation_context.is_empty() {
            msg.push_str("\n\n## Consultation Context\n\n");
            msg.push_str(consultation_context);
        }
        msg
    }
}

impl Agent for AdrDecomposerAgent {
    fn role(&self) -> &str {
        "adr_decomposer"
    }

    fn persona(&self) -> &str {
        "Cartographer"
    }

    async fn execute(&self, ctx: &AgentContext) -> error::Result<AgentOutput> {
        let messages = vec![
            Message {
                role: MessageRole::System,
                content: MessageContent::Text(DECOMPOSER_SYSTEM_PROMPT.into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text(ctx.objective.clone()),
            },
        ];

        let json_fmt = json_response_format();
        let response = self
            .router
            .complete("adr_decomposer", &messages, 0.2, 8192, Some(&json_fmt))
            .await?;

        let metadata = AgentMetadata {
            agent: "adr_decomposer".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        let fallback = serde_json::json!({
            "adr_filename": "",
            "adr_title": "",
            "summary": "parse_error",
            "beads": [],
            "security_concerns": [],
            "implementation_notes": "",
            "confidence": 0.0
        });

        Ok(parse_json_response(&response.content, metadata, fallback))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::test_helpers::{
        SequentialMockProvider, final_response, mock_router_ref, test_agent_context,
    };
    use glitchlab_kernel::agent::Agent;

    fn decomposer_router_ref() -> RouterRef {
        use crate::agents::test_helpers::MockProvider;
        use glitchlab_kernel::budget::BudgetTracker;
        use std::collections::HashMap;
        use std::sync::Arc;

        let routing = HashMap::from([
            ("adr_decomposer".to_string(), "mock/test".to_string()),
            ("planner".to_string(), "mock/test".to_string()),
            ("implementer".to_string(), "mock/test".to_string()),
            ("debugger".to_string(), "mock/test".to_string()),
            ("security".to_string(), "mock/test".to_string()),
            ("release".to_string(), "mock/test".to_string()),
            ("archivist".to_string(), "mock/test".to_string()),
            ("architect_triage".to_string(), "mock/test".to_string()),
            ("architect_review".to_string(), "mock/test".to_string()),
            ("ops_diagnosis".to_string(), "mock/test".to_string()),
            ("ciso".to_string(), "mock/test".to_string()),
            ("backlog_review".to_string(), "mock/test".to_string()),
        ]);
        let budget = BudgetTracker::new(1_000_000, 100.0);
        let mut router = glitchlab_router::Router::new(routing, budget);
        router.register_provider("mock".into(), Arc::new(MockProvider));
        Arc::new(router)
    }

    #[test]
    fn role_and_persona() {
        let agent = AdrDecomposerAgent::new(mock_router_ref());
        assert_eq!(agent.role(), "adr_decomposer");
        assert_eq!(agent.persona(), "Cartographer");
    }

    #[tokio::test]
    async fn execute_returns_output() {
        let agent = AdrDecomposerAgent::new(decomposer_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "adr_decomposer");
    }

    #[tokio::test]
    async fn fallback_on_parse_error() {
        use glitchlab_kernel::budget::BudgetTracker;
        use std::collections::HashMap;
        use std::sync::Arc;

        let routing = HashMap::from([
            ("adr_decomposer".to_string(), "seq/test".to_string()),
            ("planner".to_string(), "seq/test".to_string()),
            ("implementer".to_string(), "seq/test".to_string()),
            ("debugger".to_string(), "seq/test".to_string()),
            ("security".to_string(), "seq/test".to_string()),
            ("release".to_string(), "seq/test".to_string()),
            ("archivist".to_string(), "seq/test".to_string()),
            ("architect_triage".to_string(), "seq/test".to_string()),
            ("architect_review".to_string(), "seq/test".to_string()),
            ("ops_diagnosis".to_string(), "seq/test".to_string()),
            ("ciso".to_string(), "seq/test".to_string()),
            ("backlog_review".to_string(), "seq/test".to_string()),
        ]);
        let budget = BudgetTracker::new(1_000_000, 100.0);
        let mut router = glitchlab_router::Router::new(routing, budget);
        router.register_provider(
            "seq".into(),
            Arc::new(SequentialMockProvider::new(vec![final_response(
                "This is not valid JSON.",
            )])),
        );
        let router_ref = Arc::new(router);

        let agent = AdrDecomposerAgent::new(router_ref);
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert!(output.parse_error);
        assert_eq!(output.data["summary"], "parse_error");
        assert!(output.data["beads"].as_array().unwrap().is_empty());
    }

    #[test]
    fn build_prompt_includes_adr_and_beads() {
        let adr = "# ADR: Use Rust\n\n## Decision\n\nWe use Rust.\n";
        let beads = r#"[{"id": "gl-1e0.1", "title": "Existing task"}]"#;
        let prompt = AdrDecomposerAgent::build_prompt(adr, beads, "");
        assert!(prompt.contains("ADR Content"));
        assert!(prompt.contains("Use Rust"));
        assert!(prompt.contains("gl-1e0.1"));
        assert!(prompt.contains("Existing Beads"));
        assert!(!prompt.contains("Consultation Context"));
    }

    #[test]
    fn build_prompt_includes_consultation() {
        let adr = "# ADR: Use JWT\n";
        let beads = "[]";
        let consultation = "Security: Consider token expiry. Architect: Feasible.";
        let prompt = AdrDecomposerAgent::build_prompt(adr, beads, consultation);
        assert!(prompt.contains("Consultation Context"));
        assert!(prompt.contains("Consider token expiry"));
        assert!(prompt.contains("Feasible"));
    }

    #[test]
    fn system_prompt_contains_schema() {
        assert!(DECOMPOSER_SYSTEM_PROMPT.contains("adr_filename"));
        assert!(DECOMPOSER_SYSTEM_PROMPT.contains("beads"));
        assert!(DECOMPOSER_SYSTEM_PROMPT.contains("id_suffix"));
        assert!(DECOMPOSER_SYSTEM_PROMPT.contains("confidence"));
        assert!(DECOMPOSER_SYSTEM_PROMPT.contains("security_concerns"));
        assert!(DECOMPOSER_SYSTEM_PROMPT.contains("Superseded"));
        assert!(DECOMPOSER_SYSTEM_PROMPT.contains("Rejected"));
    }
}
