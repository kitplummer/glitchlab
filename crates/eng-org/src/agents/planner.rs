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

## Token budget constraint

The implementer has a STRICT budget of ~120K tokens per task (~9 tool turns).
A single file read costs 1-3K tokens. A test + build cycle costs 3-5K tokens.
Each turn costs ~3-5K tokens. Budget is tight — do not explore.

## File-size-aware complexity

File count alone does NOT determine complexity. A 2-file task touching a
1000-line file is harder than a 4-file task touching four 30-line files.

When assessing complexity, consider the total lines across affected files:
- <200 lines total: fits comfortably in one pass
- 200-500 lines: tight but feasible in one pass with precise read hints
- 500+ lines: likely needs decomposition OR very precise line-range scoping

In your `steps`, include a `read_hint` for each file specifying which section
the implementer needs: e.g. "Read lines 100-180 of router.rs (the Router struct
and its impl block)". This prevents the implementer from reading entire large
files and burning budget on irrelevant context.

## Task decomposition

FIRST, assess complexity honestly:
- "trivial": 1 file, 1-2 edits, mechanical change (add Display impl, rename, config tweak)
- "small": 1 file, 2-3 edits, requires reading but not design (add function, add test)
- "medium": 2-3 files, requires design decisions (new module, cross-file refactor)
- "large": 4+ files, architectural change, touches multiple modules

DO NOT DECOMPOSE when estimated_complexity is "trivial" or "small".
These tasks fit in a single implementer pass. Decomposing them wastes tokens on
sub-task overhead (workspace setup, context assembly) that exceeds the work itself.

You MUST decompose ONLY when ALL of these conditions are true:
- estimated_complexity is "medium" or "large"
- The task would touch 4+ files AND require 5+ steps,
  OR the total lines across affected files exceeds 500
- No single implementer pass could complete it within 9 tool turns

When decomposing, set `estimated_complexity` to "medium" or "large" and add a `decomposition` array:

{
  "estimated_complexity": "medium",
  "task_type": "doc|refactor|bugfix|new_code|test",
  "decomposition": [
    {
      "id": "<parent-id>-part1",
      "objective": "<focused objective — 1 file, 1-2 edits>",
      "files_likely_affected": ["<path>"],
      "depends_on": []
    },
    {
      "id": "<parent-id>-part2",
      "objective": "<next piece, may depend on part1>",
      "files_likely_affected": ["<path>"],
      "depends_on": ["<parent-id>-part1"]
    }
  ],
  ... (other fields still required)
}

Each sub-task MUST:
- Touch at most 1-2 files
- Require at most 2-3 edits
- Be completable within ~9 tool turns
- Include specific file paths and line-range hints (e.g. "lines 200-250")
- Include `files_likely_affected` listing the exact files it will touch

When decomposing, the `steps` array should be empty (the sub-tasks replace it).

Rules:
- Keep steps minimal and atomic.
- List ALL files that will be touched.
- If the task is ambiguous, choose the simplest interpretation.
- If the task requires changes to protected paths, set requires_core_change to true.
  Protected paths CANNOT be modified — the tool dispatcher will reject all writes.
  If the ENTIRE task requires modifying only protected paths, return an empty `steps`
  array with `risk_notes` explaining which paths are protected and why the task is
  infeasible. If only PART of the task touches protected paths, decompose it so that
  the feasible parts can proceed independently.
- Default to NOT decomposing. Only decompose when the task genuinely cannot fit in one pass.
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

    #[test]
    fn system_prompt_contains_file_size_awareness() {
        assert!(
            SYSTEM_PROMPT.contains("File-size-aware complexity"),
            "prompt should have file-size-aware section"
        );
        assert!(
            SYSTEM_PROMPT.contains("File count alone does NOT determine complexity"),
            "prompt should warn about file count"
        );
        assert!(
            SYSTEM_PROMPT.contains("500+ lines"),
            "prompt should mention 500+ lines threshold"
        );
        assert!(
            SYSTEM_PROMPT.contains("read_hint"),
            "prompt should mention read_hint for steps"
        );
        assert!(
            SYSTEM_PROMPT.contains("total lines across affected files exceeds 500"),
            "decomposition conditions should include line count"
        );
    }

    #[test]
    fn system_prompt_contains_budget() {
        assert!(
            SYSTEM_PROMPT.contains("~120K tokens"),
            "prompt should reference 120K budget"
        );
    }
}
