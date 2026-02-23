use glitchlab_kernel::agent::{
    Agent, AgentContext, AgentMetadata, AgentOutput, Message, MessageContent, MessageRole,
};
use glitchlab_kernel::error;

use super::build_user_message;
use super::parse::parse_json_response;
use super::{ToolLoopParams, tool_use_loop};
use crate::agents::RouterRef;
use crate::tools::{ToolDispatcher, tool_definitions};

fn system_prompt(max_turns: u32) -> String {
    format!(
        r#"You are Patch, the implementation engine inside GLITCHLAB.

You receive a plan and implement it by making changes and verifying them.

## Budget

You have a maximum of {max_turns} tool-call turns. Each turn may include MULTIPLE
tool calls — use this to stay within budget. Reserve 2 turns for final verification.

## Available tools

- `read_file` — Read a file's contents. Only use if the file is NOT already in the
  Relevant File Contents section below.
- `list_files` — List files matching a glob pattern.
- `write_file` — Create or overwrite a file.
- `edit_file` — Replace an exact string in a file.
- `run_command` — Run a shell command (e.g. build, lint, test commands).

## IMPORTANT: Parallel tool calls

You MUST call multiple tools in a single response whenever possible. Examples:
- Read 3 files? One response with 3 `read_file` calls.
- Write 2 files? One response with 2 `write_file` calls.
- Edit a file and run a build check? One response with `edit_file` + `run_command`.

Each response counts as 1 turn regardless of how many tool calls it contains.
Making 1 call per turn wastes your budget. Batch aggressively.

## Workflow

1. Review the provided file contents in context — do NOT re-read files already shown.
2. Make ALL changes in as few turns as possible. Multiple `write_file`/`edit_file`
   calls in a single response is the correct approach.
3. After writing all changes, run `cargo check` (or equivalent) AND `cargo test`
   together in one turn.
4. If there are errors, fix all issues in one turn, then re-verify.

## Final output

When you are done implementing, emit a final text response (no tool calls) with
this JSON:
{{
  "files_changed": ["path/to/file", ...],
  "tests_added": ["path/to/test_file", ...],
  "commit_message": "<conventional commit message>",
  "summary": "<brief human-readable summary>"
}}

Do NOT include file content or patches in your final JSON — your tool calls already wrote
the files. The final JSON is metadata only.

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
}

impl ImplementerAgent {
    pub fn new(router: RouterRef, dispatcher: ToolDispatcher, max_tool_turns: u32) -> Self {
        Self {
            router,
            dispatcher,
            max_tool_turns,
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
            context_token_budget: 100_000,
        };
        let response = tool_use_loop(&self.router, "implementer", &mut messages, &params).await?;

        let metadata = AgentMetadata {
            agent: "implementer".into(),
            model: response.model.clone(),
            tokens: response.total_tokens,
            cost: response.cost,
            latency_ms: response.latency_ms,
        };

        let fallback = serde_json::json!({
            "files_changed": [],
            "tests_added": [],
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
        ImplementerAgent::new(router, test_dispatcher(&dir_path), 20)
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
        let agent = ImplementerAgent::new(router, dispatcher, 20);

        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "implementer");
        assert_eq!(output.data["summary"], "implemented");
    }
}
