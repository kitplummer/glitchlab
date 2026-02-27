use glitchlab_kernel::agent::{
    Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageContent, MessageRole,
};
use glitchlab_kernel::error;
use tracing::warn;

use super::build_user_message;
use super::parse::parse_json_response;
use super::{StuckReason, ToolLoopOutcome, ToolLoopParams, tool_use_loop};
use crate::agents::RouterRef;
use crate::tools::{ToolDispatcher, tool_definitions};

fn system_prompt(max_turns: u32) -> String {
    format!(
        r#"You are Patch, the implementation engine inside GLITCHLAB.

You receive a plan and implement it by making changes and verifying them.

## Budget

You have a STRICT maximum of {max_turns} tool-call turns.
Each turn costs ~3-5K tokens. Budget is tight — do not explore.
Each turn may include MULTIPLE tool calls — batch aggressively to stay within budget.
Reserve 2 turns for final verification. You WILL be terminated if you exceed budget.

## Available tools

- `read_file` — Read a file's contents. Supports optional `start_line` and `end_line`
  parameters to read a specific range (1-based, inclusive). For large files (>200 lines),
  you MUST use line ranges from the plan's read hints. Never read an entire large file
  when the plan specifies which section you need. Only use if the file is NOT already
  in the Relevant File Contents section below.
- `list_files` — List files matching a glob pattern (e.g. "crates/**/*.rs", "src/*.ts").
  Only use if the "Codebase Overview" and "Rust Module Map" sections below don't already
  answer your question. These sections tell you where files are — do NOT re-discover
  with `list_files` what is already provided.
- `write_file` — Create or overwrite a file.
- `edit_file` — Replace an exact string in a file. The `old_string` must be a UNIQUE,
  EXACT match of existing content (copy it from `read_file` output). Include enough
  surrounding context lines to ensure uniqueness.
- `run_command` — Run a shell command. Allowed: build, test, lint, git, and read-only
  commands (find, ls, grep, head, tail, cat). NOT allowed: curl, wget, sudo, rm -rf.

## IMPORTANT: Parallel tool calls

You MUST call multiple tools in a single response whenever possible. Examples:
- Read 3 files? One response with 3 `read_file` calls.
- Write 2 files? One response with 2 `write_file` calls.
- Edit a file and run a build check? One response with `edit_file` + `run_command`.

Each response counts as 1 turn regardless of how many tool calls it contains.
Making 1 call per turn wastes your budget. Batch aggressively.

## Workflow (STRICT ORDER)

1. **Write tests first.** Create or update test files that cover the planned changes.
   Run them with `cargo test` (or equivalent) — they MUST fail (red).
2. **Implement.** Write the minimum code to make the tests pass. Batch file edits;
   do NOT re-read files already shown in the Relevant File Contents section.
3. **Verify.** Run `cargo test` — all tests MUST pass (green).
4. **Lint.** Run `cargo clippy -- -D warnings` and `cargo fmt --check`. Fix any issues.

Do NOT skip step 1. Do NOT write implementation before tests.

## Final output

When you are done implementing, emit a final text response (no tool calls) with
this JSON:
{{
  "files_changed": ["path/to/file", ...],
  "tests_added": ["path/to/test_file", ...],
  "tests_passing": <bool>,
  "commit_message": "<conventional commit message>",
  "summary": "<brief human-readable summary>"
}}

Do NOT include file content or patches in your final JSON — your tool calls already wrote
the files. The final JSON is metadata only.

## Protected paths

Some paths in the repository are protected by project policy and CANNOT be modified.
If a tool call fails with a message containing "PROTECTED PATH", do NOT retry the same
edit. Instead, stop immediately and report the issue in your final JSON with
`"tests_passing": false` and a `"summary"` explaining which path was protected.

Rules:
- Follow the plan exactly. No feature creep.
- Keep diffs minimal. Always add/update tests.
- Use idiomatic patterns for the language.
- CRITICAL: Output ONLY the raw JSON object in your final response. No markdown, no text."#
    )
}

pub struct ImplementerAgent {
    router: RouterRef,
    dispatcher: ToolDispatcher,
    max_tool_turns: u32,
    max_stuck_turns: u32,
}

impl ImplementerAgent {
    pub fn new(
        router: RouterRef,
        dispatcher: ToolDispatcher,
        max_tool_turns: u32,
        max_stuck_turns: u32,
    ) -> Self {
        Self {
            router,
            dispatcher,
            max_tool_turns,
            max_stuck_turns,
        }
    }
}

impl Agent for ImplementerAgent {
    fn role(&self) -> &str {
        "implementer"
    }

    fn persona(&self) -> &str {
        "Patch"
    }

    async fn execute(&self, ctx: &AgentContext) -> error::Result<AgentOutput> {
        let mut messages = vec![
            Message {
                role: MessageRole::System,
                content: MessageContent::Text(system_prompt(self.max_tool_turns)),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text(build_user_message(ctx)),
            },
        ];

        let tool_defs = tool_definitions();
        let params = ToolLoopParams {
            tool_defs: &tool_defs,
            dispatcher: &self.dispatcher,
            max_turns: self.max_tool_turns,
            temperature: 0.2,
            max_tokens: 16384,
            context_token_budget: 8_000,
            max_stuck_turns: self.max_stuck_turns,
        };
        let outcome = tool_use_loop(&self.router, "implementer", &mut messages, &params).await?;

        let (response, stuck_reason) = match outcome {
            ToolLoopOutcome::Completed(r) => (r, None),
            ToolLoopOutcome::Stuck {
                last_response,
                reason,
            } => {
                warn!(?reason, "implementer agent stuck");
                (last_response, Some(reason))
            }
        };

        let metadata = AgentMetadata {
            agent: "implementer".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        if let Some(reason) = stuck_reason {
            let reason_str = match reason {
                StuckReason::RepeatedResults => "repeated_results",
                StuckReason::ConsecutiveErrors => "consecutive_errors",
                StuckReason::BoundaryViolation => "boundary_violation",
            };
            return Ok(AgentOutput {
                data: serde_json::json!({
                    "files_changed": [],
                    "tests_added": [],
                    "tests_passing": false,
                    "commit_message": "chore: no changes produced",
                    "summary": "Implementer agent stuck",
                    "stuck": true,
                    "stuck_reason": reason_str,
                }),
                metadata,
                parse_error: true,
            });
        }

        let fallback = serde_json::json!({
            "files_changed": [],
            "tests_added": [],
            "tests_passing": false,
            "commit_message": "chore: no changes produced",
            "summary": "Failed to parse implementer output"
        });

        Ok(parse_json_response(&response.content, metadata, fallback))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::test_helpers::{
        final_response, mock_router_ref, sequential_router_ref, test_agent_context,
        test_dispatcher, tool_response,
    };
    use glitchlab_kernel::agent::Agent;
    use glitchlab_kernel::tool::ToolCall;

    fn make_agent(router: RouterRef) -> ImplementerAgent {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so it lives for the test duration.
        let dir_path = dir.keep();
        ImplementerAgent::new(router, test_dispatcher(&dir_path), 20, 3)
    }

    #[test]
    fn role_and_persona() {
        let agent = make_agent(mock_router_ref());
        assert_eq!(agent.role(), "implementer");
        assert_eq!(agent.persona(), "Patch");
    }

    #[tokio::test]
    async fn execute_returns_output() {
        let agent = make_agent(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "implementer");
    }

    #[tokio::test]
    async fn execute_with_previous_output() {
        let agent = make_agent(mock_router_ref());
        let mut ctx = test_agent_context();
        ctx.previous_output = serde_json::json!({"steps": [{"description": "add feature"}]});
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "implementer");
    }

    #[test]
    fn system_prompt_contains_turn_count() {
        let prompt = system_prompt(15);
        assert!(
            prompt.contains("15 tool-call turns"),
            "prompt should embed the turn count"
        );
        assert!(
            !prompt.contains("explore the codebase"),
            "prompt should not encourage exploration"
        );
        assert!(
            prompt.contains("do NOT re-read files already shown"),
            "prompt should discourage redundant reads"
        );
        assert!(
            prompt.contains("Parallel tool calls"),
            "prompt should encourage parallel tool calls"
        );
        assert!(
            prompt.contains("MUST call multiple tools"),
            "prompt should be explicit about batching"
        );
    }

    #[test]
    fn system_prompt_contains_module_map_hint() {
        let prompt = system_prompt(10);
        assert!(
            prompt.contains("Rust Module Map"),
            "prompt should reference Rust Module Map"
        );
        assert!(
            prompt.contains("Codebase Overview"),
            "prompt should reference Codebase Overview for orientation"
        );
    }

    #[test]
    fn system_prompt_contains_tdd_workflow() {
        let prompt = system_prompt(10);
        assert!(
            prompt.contains("Write tests first"),
            "prompt should enforce test-first ordering"
        );
        assert!(
            prompt.contains("STRICT ORDER"),
            "prompt should label workflow as strict"
        );
        assert!(
            prompt.contains("Do NOT skip step 1"),
            "prompt should forbid skipping tests"
        );
    }

    #[test]
    fn system_prompt_contains_read_hints() {
        let prompt = system_prompt(10);
        assert!(
            prompt.contains("MUST use line ranges from the plan's read hints"),
            "prompt should enforce read hints for large files"
        );
        assert!(
            prompt.contains("Never read an entire large file"),
            "prompt should forbid reading entire large files when hints exist"
        );
    }

    #[test]
    fn system_prompt_contains_red_green() {
        let prompt = system_prompt(10);
        assert!(
            prompt.contains("MUST fail"),
            "prompt should require red phase"
        );
        assert!(
            prompt.contains("MUST pass"),
            "prompt should require green phase"
        );
        assert!(
            prompt.contains("tests_passing"),
            "prompt schema should include tests_passing"
        );
    }

    #[tokio::test]
    async fn execute_with_tool_use() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn old() {}").unwrap();
        let dispatcher = test_dispatcher(dir.path());

        let responses = vec![
            // Turn 1: read a file
            tool_response(vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "lib.rs"}),
            }]),
            // Turn 2: final response
            final_response(
                r#"{"files_changed": ["lib.rs"], "tests_added": [], "commit_message": "feat: done", "summary": "implemented"}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let agent = ImplementerAgent::new(router, dispatcher, 20, 3);

        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "implementer");
        assert_eq!(output.data["summary"], "implemented");
    }

    #[tokio::test]
    async fn execute_stuck_returns_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "same content").unwrap();
        let dispatcher = test_dispatcher(dir.path());

        // 5 identical tool calls reading the same file — triggers stuck detection.
        let responses: Vec<glitchlab_router::RouterResponse> = (0..5)
            .map(|i| {
                tool_response(vec![ToolCall {
                    id: format!("c{i}"),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "f.txt"}),
                }])
            })
            .collect();
        let router = sequential_router_ref(responses);
        let agent = ImplementerAgent::new(router, dispatcher, 20, 3);

        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert!(output.parse_error, "stuck should set parse_error");
        assert_eq!(output.data["stuck"], true);
        assert_eq!(output.data["stuck_reason"], "repeated_results");
        assert_eq!(output.data["should_retry"], serde_json::Value::Null);
    }
}
