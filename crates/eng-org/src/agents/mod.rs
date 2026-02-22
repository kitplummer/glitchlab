pub mod archivist;
pub mod debugger;
pub mod implementer;
pub mod planner;
pub mod release;
pub mod security;

mod parse;

use std::sync::Arc;

use glitchlab_kernel::agent::{AgentContext, ContentBlock, Message, MessageContent, MessageRole};
use glitchlab_kernel::error;
use glitchlab_kernel::tool::ToolDefinition;
use glitchlab_router::RouterResponse;
use tracing::warn;

use crate::tools::ToolDispatcher;

/// Shared reference to the router, used by all agents.
pub type RouterRef = Arc<glitchlab_router::Router>;

/// Parameters for the tool-use execution loop.
pub(crate) struct ToolLoopParams<'a> {
    pub tool_defs: &'a [ToolDefinition],
    pub dispatcher: &'a ToolDispatcher,
    pub max_turns: u32,
    pub temperature: f32,
    pub max_tokens: u32,
}

/// Shared tool-use execution loop for agents that support iterative tool use.
///
/// Calls the LLM in a loop, executing tool calls via `dispatcher` and feeding
/// results back until the LLM stops requesting tools or `max_turns` is reached.
pub(crate) async fn tool_use_loop(
    router: &RouterRef,
    role: &str,
    messages: &mut Vec<Message>,
    params: &ToolLoopParams<'_>,
) -> error::Result<RouterResponse> {
    let mut turns = 0u32;

    loop {
        let response = router
            .complete_with_tools(
                role,
                messages,
                params.temperature,
                params.max_tokens,
                params.tool_defs,
                None,
            )
            .await?;

        // If no tool calls, the LLM is done — return the final response.
        if response.tool_calls.is_empty() {
            return Ok(response);
        }

        // Build the assistant message with text + tool-use blocks.
        let mut blocks = Vec::new();
        if !response.content.is_empty() {
            blocks.push(ContentBlock::Text {
                text: response.content.clone(),
            });
        }
        for call in &response.tool_calls {
            blocks.push(ContentBlock::ToolUse(call.clone()));
        }
        messages.push(Message {
            role: MessageRole::Assistant,
            content: MessageContent::Blocks(blocks),
        });

        // Execute each tool call and append results as user messages.
        for call in &response.tool_calls {
            let result = params.dispatcher.dispatch(call).await;
            messages.push(Message {
                role: MessageRole::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult(result)]),
            });
        }

        turns += 1;
        if turns >= params.max_turns {
            warn!(
                role,
                turns,
                max_turns = params.max_turns,
                "tool-use loop hit max turns, returning last response"
            );
            return Ok(response);
        }
    }
}

/// Build the full user message from agent context, incorporating previous
/// stage output, file contents, and constraints alongside the objective.
///
/// This is used by all agents so they can see each other's output across
/// pipeline stages.
pub(crate) fn build_user_message(ctx: &AgentContext) -> String {
    let mut msg = ctx.objective.clone();

    // Previous stage output (the critical fix for agent context blindness).
    if !ctx.previous_output.is_null() {
        msg.push_str("\n\n## Previous Stage Output\n\n");
        // Pretty-print for readability in the LLM context window.
        msg.push_str(
            &serde_json::to_string_pretty(&ctx.previous_output)
                .unwrap_or_else(|_| ctx.previous_output.to_string()),
        );
    }

    // File contents relevant to the task.
    if !ctx.file_context.is_empty() {
        msg.push_str("\n\n## Relevant File Contents\n");
        let mut paths: Vec<_> = ctx.file_context.keys().collect();
        paths.sort();
        for path in paths {
            let content = &ctx.file_context[path];
            msg.push_str(&format!("\n### `{path}`\n\n```\n{content}\n```\n"));
        }
    }

    // Failure history (stored separately so ContextAssembler can drop it independently).
    if let Some(serde_json::Value::String(fh)) = ctx.extra.get("failure_history")
        && !fh.is_empty()
    {
        msg.push_str("\n\n## Failure History\n\n");
        msg.push_str(fh);
    }

    // Task constraints.
    if !ctx.constraints.is_empty() {
        msg.push_str("\n\n## Constraints\n\n");
        for constraint in &ctx.constraints {
            msg.push_str(&format!("- {constraint}\n"));
        }
    }

    msg
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;
    use glitchlab_kernel::agent::{AgentContext, Message};
    use glitchlab_kernel::budget::BudgetTracker;
    use glitchlab_kernel::tool::{ToolCall, ToolDefinition};
    use glitchlab_router::RouterResponse;
    use glitchlab_router::provider::{Provider, ProviderFuture};
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use std::time::Duration;

    use crate::tools::ToolDispatcher;
    use glitchlab_kernel::tool::ToolPolicy;

    pub(crate) struct MockProvider;

    impl Provider for MockProvider {
        fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _temperature: f32,
            _max_tokens: u32,
            _response_format: Option<&serde_json::Value>,
        ) -> ProviderFuture<'_> {
            Box::pin(async move {
                Ok(RouterResponse {
                    content: r#"{"result": "ok", "steps": [{"step_number": 1, "description": "add feature", "files": ["src/new.rs"], "action": "create"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "trivial", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false, "verdict": "pass", "issues": [], "version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": [], "diagnosis": "none", "root_cause": "none", "fix": {"changes": []}, "confidence": "high", "should_retry": false, "changes": [{"file": "src/new.rs", "action": "create", "content": "pub fn greet() -> &'static str { \"hello\" }\n", "description": "add greet function"}], "tests_added": [], "commit_message": "feat: add greet function", "summary": "test", "adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#.into(),
                    model: "mock/test-model".into(),
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    cost: 0.001,
                    latency_ms: 42,
                    tool_calls: vec![],
                    stop_reason: None,
                })
            })
        }
    }

    /// Mock provider that returns pre-scripted responses in sequence.
    /// Used by tool_use_loop tests to simulate multi-turn conversations.
    pub(crate) struct SequentialMockProvider {
        responses: Mutex<VecDeque<RouterResponse>>,
    }

    impl SequentialMockProvider {
        pub(crate) fn new(responses: Vec<RouterResponse>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }
    }

    impl Provider for SequentialMockProvider {
        fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _temperature: f32,
            _max_tokens: u32,
            _response_format: Option<&serde_json::Value>,
        ) -> ProviderFuture<'_> {
            let response = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| RouterResponse {
                    content: "no more responses".into(),
                    model: "mock/test".into(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    cost: 0.0,
                    latency_ms: 0,
                    tool_calls: vec![],
                    stop_reason: None,
                });
            Box::pin(async move { Ok(response) })
        }

        fn complete_with_tools(
            &self,
            model: &str,
            messages: &[Message],
            temperature: f32,
            max_tokens: u32,
            _tools: &[ToolDefinition],
            response_format: Option<&serde_json::Value>,
        ) -> ProviderFuture<'_> {
            self.complete(model, messages, temperature, max_tokens, response_format)
        }
    }

    /// Build a RouterRef backed by a SequentialMockProvider.
    pub(crate) fn sequential_router_ref(responses: Vec<RouterResponse>) -> RouterRef {
        let routing = HashMap::from([
            ("planner".to_string(), "seq/test".to_string()),
            ("implementer".to_string(), "seq/test".to_string()),
            ("debugger".to_string(), "seq/test".to_string()),
            ("security".to_string(), "seq/test".to_string()),
            ("release".to_string(), "seq/test".to_string()),
            ("archivist".to_string(), "seq/test".to_string()),
        ]);
        let budget = BudgetTracker::new(1_000_000, 100.0);
        let mut router = glitchlab_router::Router::new(routing, budget);
        router.register_provider(
            "seq".into(),
            Arc::new(SequentialMockProvider::new(responses)),
        );
        Arc::new(router)
    }

    /// Create a simple RouterResponse with no tool calls.
    pub(crate) fn final_response(content: &str) -> RouterResponse {
        RouterResponse {
            content: content.into(),
            model: "mock/test".into(),
            prompt_tokens: 50,
            completion_tokens: 25,
            total_tokens: 75,
            cost: 0.0005,
            latency_ms: 20,
            tool_calls: vec![],
            stop_reason: Some("end_turn".into()),
        }
    }

    /// Create a RouterResponse with tool calls.
    pub(crate) fn tool_response(calls: Vec<ToolCall>) -> RouterResponse {
        RouterResponse {
            content: String::new(),
            model: "mock/test".into(),
            prompt_tokens: 50,
            completion_tokens: 25,
            total_tokens: 75,
            cost: 0.0005,
            latency_ms: 20,
            tool_calls: calls,
            stop_reason: Some("tool_use".into()),
        }
    }

    pub(crate) fn mock_router_ref() -> RouterRef {
        let routing = HashMap::from([
            ("planner".to_string(), "mock/test".to_string()),
            ("implementer".to_string(), "mock/test".to_string()),
            ("debugger".to_string(), "mock/test".to_string()),
            ("security".to_string(), "mock/test".to_string()),
            ("release".to_string(), "mock/test".to_string()),
            ("archivist".to_string(), "mock/test".to_string()),
        ]);
        let budget = BudgetTracker::new(1_000_000, 100.0);
        let mut router = glitchlab_router::Router::new(routing, budget);
        router.register_provider("mock".into(), Arc::new(MockProvider));
        Arc::new(router)
    }

    /// Create a ToolDispatcher rooted at a given directory.
    pub(crate) fn test_dispatcher(dir: &std::path::Path) -> ToolDispatcher {
        ToolDispatcher::new(
            dir.to_path_buf(),
            ToolPolicy::new(
                vec![
                    "echo".into(),
                    "cat".into(),
                    "cargo check".into(),
                    "cargo test".into(),
                ],
                vec!["rm".into()],
            ),
            vec![],
            Duration::from_secs(10),
        )
    }

    pub(crate) fn test_agent_context() -> AgentContext {
        AgentContext {
            task_id: "test-1".into(),
            objective: "Fix the bug in auth module".into(),
            repo_path: "/tmp/repo".into(),
            working_dir: "/tmp/repo/worktree".into(),
            constraints: vec![],
            acceptance_criteria: vec![],
            risk_level: "low".into(),
            file_context: HashMap::new(),
            previous_output: serde_json::Value::Null,
            extra: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn base_ctx() -> AgentContext {
        AgentContext {
            task_id: "t1".into(),
            objective: "Do the thing".into(),
            repo_path: "/repo".into(),
            working_dir: "/repo/wt".into(),
            constraints: vec![],
            acceptance_criteria: vec![],
            risk_level: "low".into(),
            file_context: HashMap::new(),
            previous_output: serde_json::Value::Null,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn build_user_message_objective_only() {
        let ctx = base_ctx();
        let msg = build_user_message(&ctx);
        assert_eq!(msg, "Do the thing");
    }

    #[test]
    fn build_user_message_with_previous_output() {
        let mut ctx = base_ctx();
        ctx.previous_output = serde_json::json!({"plan": "step 1"});
        let msg = build_user_message(&ctx);
        assert!(msg.contains("## Previous Stage Output"));
        assert!(msg.contains("step 1"));
    }

    #[test]
    fn build_user_message_with_file_context() {
        let mut ctx = base_ctx();
        ctx.file_context
            .insert("src/lib.rs".into(), "fn hello() {}".into());
        let msg = build_user_message(&ctx);
        assert!(msg.contains("## Relevant File Contents"));
        assert!(msg.contains("`src/lib.rs`"));
        assert!(msg.contains("fn hello() {}"));
    }

    #[test]
    fn build_user_message_with_constraints() {
        let mut ctx = base_ctx();
        ctx.constraints = vec!["No new deps".into(), "Keep it simple".into()];
        let msg = build_user_message(&ctx);
        assert!(msg.contains("## Constraints"));
        assert!(msg.contains("- No new deps"));
        assert!(msg.contains("- Keep it simple"));
    }

    #[test]
    fn build_user_message_all_populated() {
        let mut ctx = base_ctx();
        ctx.previous_output = serde_json::json!({"result": "ok"});
        ctx.file_context
            .insert("Cargo.toml".into(), "[package]".into());
        ctx.constraints = vec!["Stay safe".into()];

        let msg = build_user_message(&ctx);
        assert!(msg.starts_with("Do the thing"));
        assert!(msg.contains("## Previous Stage Output"));
        assert!(msg.contains("## Relevant File Contents"));
        assert!(msg.contains("## Constraints"));
    }

    #[test]
    fn build_user_message_with_failure_history() {
        let mut ctx = base_ctx();
        ctx.extra.insert(
            "failure_history".into(),
            serde_json::Value::String("Recent failures to avoid repeating:\n- Task `t1` failed with status `error`: crash\n".into()),
        );
        let msg = build_user_message(&ctx);
        assert!(msg.contains("## Failure History"));
        assert!(msg.contains("Task `t1` failed"));
        assert!(msg.contains("crash"));
    }

    #[test]
    fn build_user_message_empty_failure_history_omitted() {
        let mut ctx = base_ctx();
        ctx.extra.insert(
            "failure_history".into(),
            serde_json::Value::String(String::new()),
        );
        let msg = build_user_message(&ctx);
        assert!(!msg.contains("## Failure History"));
    }

    // --- tool_use_loop tests ---

    use glitchlab_kernel::tool::ToolCall;
    use test_helpers::{final_response, sequential_router_ref, test_dispatcher, tool_response};

    fn tool_call(id: &str, name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    #[tokio::test]
    async fn tool_use_loop_no_tool_calls_returns_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let dispatcher = test_dispatcher(dir.path());
        let router = sequential_router_ref(vec![final_response(r#"{"done": true}"#)]);
        let tool_defs = crate::tools::tool_definitions();
        let mut messages = vec![Message {
            role: MessageRole::User,
            content: MessageContent::Text("hello".into()),
        }];

        let params = ToolLoopParams {
            tool_defs: &tool_defs,
            dispatcher: &dispatcher,
            max_turns: 20,
            temperature: 0.2,
            max_tokens: 4096,
        };
        let resp = tool_use_loop(&router, "implementer", &mut messages, &params)
            .await
            .unwrap();

        assert!(resp.tool_calls.is_empty());
        assert!(resp.content.contains("done"));
        // Messages should still be just the original user message.
        assert_eq!(messages.len(), 1);
    }

    #[tokio::test]
    async fn tool_use_loop_single_tool_turn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hello world").unwrap();
        let dispatcher = test_dispatcher(dir.path());

        let responses = vec![
            // Turn 1: LLM requests read_file
            tool_response(vec![tool_call(
                "call_1",
                "read_file",
                serde_json::json!({"path": "hello.txt"}),
            )]),
            // Turn 2: LLM returns final text
            final_response(r#"{"summary": "read the file"}"#),
        ];
        let router = sequential_router_ref(responses);
        let tool_defs = crate::tools::tool_definitions();
        let mut messages = vec![Message {
            role: MessageRole::User,
            content: MessageContent::Text("read hello.txt".into()),
        }];

        let params = ToolLoopParams {
            tool_defs: &tool_defs,
            dispatcher: &dispatcher,
            max_turns: 20,
            temperature: 0.2,
            max_tokens: 4096,
        };
        let resp = tool_use_loop(&router, "implementer", &mut messages, &params)
            .await
            .unwrap();

        assert!(resp.tool_calls.is_empty());
        // Messages should have grown: user + assistant(tool_use) + user(tool_result)
        assert_eq!(messages.len(), 3);

        // Check assistant message has ToolUse block
        assert_eq!(messages[1].role, MessageRole::Assistant);
        if let MessageContent::Blocks(blocks) = &messages[1].content {
            assert!(blocks.iter().any(|b| matches!(b, ContentBlock::ToolUse(_))));
        } else {
            panic!("expected Blocks content on assistant message");
        }

        // Check tool result message
        assert_eq!(messages[2].role, MessageRole::User);
        if let MessageContent::Blocks(blocks) = &messages[2].content {
            assert!(
                blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult(_)))
            );
        } else {
            panic!("expected Blocks content on tool result message");
        }
    }

    #[tokio::test]
    async fn tool_use_loop_max_turns_enforced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "data").unwrap();
        let dispatcher = test_dispatcher(dir.path());

        // Every response returns a tool call — the loop should stop at max_turns.
        let responses: Vec<RouterResponse> = (0..5)
            .map(|i| {
                tool_response(vec![tool_call(
                    &format!("call_{i}"),
                    "read_file",
                    serde_json::json!({"path": "f.txt"}),
                )])
            })
            .collect();
        let router = sequential_router_ref(responses);
        let tool_defs = crate::tools::tool_definitions();
        let mut messages = vec![Message {
            role: MessageRole::User,
            content: MessageContent::Text("loop forever".into()),
        }];

        let params = ToolLoopParams {
            tool_defs: &tool_defs,
            dispatcher: &dispatcher,
            max_turns: 3,
            temperature: 0.2,
            max_tokens: 4096,
        };
        let resp = tool_use_loop(&router, "implementer", &mut messages, &params)
            .await
            .unwrap();

        // Should have stopped after 3 turns, returning a response with tool_calls.
        assert!(!resp.tool_calls.is_empty());
        // 1 original + 3 * (assistant + tool_result) = 7
        assert_eq!(messages.len(), 7);
    }

    #[tokio::test]
    async fn tool_use_loop_multiple_tool_calls_in_one_turn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "aaa").unwrap();
        std::fs::write(dir.path().join("b.txt"), "bbb").unwrap();
        let dispatcher = test_dispatcher(dir.path());

        let responses = vec![
            // Turn 1: two tool calls
            tool_response(vec![
                tool_call("c1", "read_file", serde_json::json!({"path": "a.txt"})),
                tool_call("c2", "read_file", serde_json::json!({"path": "b.txt"})),
            ]),
            // Turn 2: done
            final_response(r#"{"done": true}"#),
        ];
        let router = sequential_router_ref(responses);
        let tool_defs = crate::tools::tool_definitions();
        let mut messages = vec![Message {
            role: MessageRole::User,
            content: MessageContent::Text("read both".into()),
        }];

        let params = ToolLoopParams {
            tool_defs: &tool_defs,
            dispatcher: &dispatcher,
            max_turns: 20,
            temperature: 0.2,
            max_tokens: 4096,
        };
        let resp = tool_use_loop(&router, "implementer", &mut messages, &params)
            .await
            .unwrap();

        assert!(resp.tool_calls.is_empty());
        // 1 user + 1 assistant(2 tool_uses) + 2 tool_results = 4 messages
        assert_eq!(messages.len(), 4);
    }

    #[tokio::test]
    async fn tool_use_loop_messages_accumulate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "content").unwrap();
        let dispatcher = test_dispatcher(dir.path());

        let responses = vec![
            tool_response(vec![tool_call(
                "c1",
                "read_file",
                serde_json::json!({"path": "f.txt"}),
            )]),
            tool_response(vec![tool_call(
                "c2",
                "list_files",
                serde_json::json!({"pattern": "*.txt"}),
            )]),
            final_response(r#"{"done": true}"#),
        ];
        let router = sequential_router_ref(responses);
        let tool_defs = crate::tools::tool_definitions();
        let mut messages = vec![Message {
            role: MessageRole::User,
            content: MessageContent::Text("explore".into()),
        }];

        let params = ToolLoopParams {
            tool_defs: &tool_defs,
            dispatcher: &dispatcher,
            max_turns: 20,
            temperature: 0.2,
            max_tokens: 4096,
        };
        tool_use_loop(&router, "implementer", &mut messages, &params)
            .await
            .unwrap();

        // 1 user + 2 * (assistant + tool_result) = 5
        assert_eq!(messages.len(), 5);

        // Verify the tool results contain actual content
        if let MessageContent::Blocks(blocks) = &messages[2].content {
            if let Some(ContentBlock::ToolResult(result)) = blocks.first() {
                assert_eq!(result.content, "content"); // read_file result
                assert!(!result.is_error);
            } else {
                panic!("expected ToolResult block");
            }
        }
    }
}
