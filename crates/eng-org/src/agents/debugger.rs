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

const SYSTEM_PROMPT: &str = r#"You are Reroute, the debug engine inside GLITCHLAB.

You are invoked ONLY when tests fail or builds break.
Produce a MINIMAL fix. Nothing more. Fix the EXACT failure.
Do not refactor, add features, or clean up code.

## Available tools

- `read_file` — Read a file's contents.
- `list_files` — List files matching a glob pattern.
- `write_file` — Create or overwrite a file.
- `edit_file` — Replace an exact string in a file.
- `run_command` — Run a shell command (e.g. `cargo check`, `cargo test`).

## Parallel tool calls

You can call multiple tools in a single response. Do this whenever possible:
- Read multiple files? One response with multiple `read_file` calls.
- Fix a file and verify? One response with `edit_file` + `run_command`.

## Workflow

1. Read the failing test output and error messages from context.
2. Use `read_file` to examine the relevant source files (batch reads in one turn).
3. Use `edit_file` to apply the minimal fix.
4. Use `run_command` to run `cargo check` or `cargo test` to verify the fix.
5. Iterate until the fix is correct.

## Final output

When you are done debugging, emit a final text response with this JSON schema:
{
  "diagnosis": "<what went wrong>",
  "root_cause": "<root cause>",
  "files_changed": ["path/to/file", ...],
  "confidence": "high|medium|low",
  "should_retry": <bool>,
  "notes": "<additional context>"
}

Do NOT include patches or file content in your final JSON — your tool calls already wrote
the fixes. The final JSON is metadata only.

Rules:
- Fix ONLY what is broken. Minimal diff.
- If you cannot diagnose the issue, set confidence to "low" and should_retry to false.
- If a previous fix attempt was provided in context, do NOT repeat it.
- CRITICAL: Output ONLY the raw JSON object in your final response. No markdown, no text."#;

pub struct DebuggerAgent {
    router: RouterRef,
    dispatcher: ToolDispatcher,
    max_tool_turns: u32,
    max_stuck_turns: u32,
}

impl DebuggerAgent {
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

impl Agent for DebuggerAgent {
    fn role(&self) -> &str {
        "debugger"
    }

    fn persona(&self) -> &str {
        "Reroute"
    }

    async fn execute(&self, ctx: &AgentContext) -> error::Result<AgentOutput> {
        let mut messages = vec![
            Message {
                role: MessageRole::System,
                content: MessageContent::Text(SYSTEM_PROMPT.into()),
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
            max_tokens: 4096,
            context_token_budget: 12_000,
            max_stuck_turns: self.max_stuck_turns,
        };
        let outcome = tool_use_loop(&self.router, "debugger", &mut messages, &params).await?;

        let (response, stuck_reason) = match outcome {
            ToolLoopOutcome::Completed(r) => (r, None),
            ToolLoopOutcome::Stuck {
                last_response,
                reason,
            } => {
                warn!(?reason, "debugger agent stuck");
                (last_response, Some(reason))
            }
        };

        let metadata = AgentMetadata {
            agent: "debugger".into(),
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
                    "diagnosis": "Debugger agent stuck",
                    "root_cause": reason_str,
                    "files_changed": [],
                    "confidence": "low",
                    "should_retry": false,
                    "notes": null,
                    "stuck": true,
                }),
                metadata,
                parse_error: true,
            });
        }

        let fallback = serde_json::json!({
            "diagnosis": "Failed to parse debugger output",
            "root_cause": "unknown",
            "files_changed": [],
            "confidence": "low",
            "should_retry": false,
            "notes": null
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

    fn make_agent(router: RouterRef) -> DebuggerAgent {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.keep();
        DebuggerAgent::new(router, test_dispatcher(&dir_path), 20, 3)
    }

    #[test]
    fn role_and_persona() {
        let agent = make_agent(mock_router_ref());
        assert_eq!(agent.role(), "debugger");
        assert_eq!(agent.persona(), "Reroute");
    }

    #[tokio::test]
    async fn execute_returns_output() {
        let agent = make_agent(mock_router_ref());
        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "debugger");
    }

    #[tokio::test]
    async fn execute_with_previous_output() {
        let agent = make_agent(mock_router_ref());
        let mut ctx = test_agent_context();
        ctx.previous_output = serde_json::json!({"test_output": "FAILED: assertion error"});
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "debugger");
    }

    #[tokio::test]
    async fn execute_with_tool_use() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bug.rs"), "fn broken() {}").unwrap();
        let dispatcher = test_dispatcher(dir.path());

        let responses = vec![
            // Turn 1: read the buggy file
            tool_response(vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "bug.rs"}),
            }]),
            // Turn 2: final diagnosis
            final_response(
                r#"{"diagnosis": "found the bug", "root_cause": "typo", "files_changed": ["bug.rs"], "confidence": "high", "should_retry": false, "notes": null}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let agent = DebuggerAgent::new(router, dispatcher, 20, 3);

        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert_eq!(output.metadata.agent, "debugger");
        assert_eq!(output.data["diagnosis"], "found the bug");
    }

    #[tokio::test]
    async fn execute_stuck_returns_should_retry_false() {
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
        let agent = DebuggerAgent::new(router, dispatcher, 20, 3);

        let ctx = test_agent_context();
        let output = agent.execute(&ctx).await.unwrap();
        assert!(output.parse_error, "stuck should set parse_error");
        assert_eq!(output.data["stuck"], true);
        assert_eq!(output.data["should_retry"], false);
        assert_eq!(output.data["root_cause"], "repeated_results");
    }
}
