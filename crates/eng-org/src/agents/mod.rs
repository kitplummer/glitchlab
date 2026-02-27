pub mod architect;
pub mod archivist;
pub mod debugger;
pub mod implementer;
pub mod ops;
pub mod planner;
pub mod release;
pub mod security;

mod parse;

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use glitchlab_kernel::agent::{AgentContext, ContentBlock, Message, MessageContent, MessageRole};
use glitchlab_kernel::error;
use glitchlab_kernel::outcome::{ObstacleKind, OutcomeContext};
use glitchlab_kernel::tool::{ToolCall, ToolCallResult, ToolDefinition};
use glitchlab_router::RouterResponse;
use tracing::{info, warn};

use crate::tools::ToolDispatcher;

/// Shared reference to the router, used by all agents.
pub type RouterRef = Arc<glitchlab_router::Router>;

/// Response format hint for JSON-only agents.
/// Providers that support structured output (OpenAI, Gemini) use this to
/// enable JSON mode, preventing markdown fences around the response.
pub(crate) fn json_response_format() -> serde_json::Value {
    serde_json::json!({"type": "json_object"})
}

/// Why the tool-use loop considers the agent stuck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StuckReason {
    /// The same tool results keep repeating (hash-based).
    RepeatedResults,
    /// Multiple consecutive turns produced errors.
    ConsecutiveErrors,
    /// A tool call hit a protected-path boundary violation.
    BoundaryViolation,
}

/// Outcome of a tool-use loop.
#[derive(Debug)]
pub(crate) enum ToolLoopOutcome {
    /// The LLM stopped requesting tools normally.
    Completed(RouterResponse),
    /// The loop was terminated because the agent appeared stuck.
    Stuck {
        reason: StuckReason,
        last_response: RouterResponse,
    },
}

/// Detects when an agent is stuck in a loop by tracking result hashes
/// and consecutive error streaks.
pub(crate) struct StuckDetector {
    max_stuck_turns: u32,
    result_hashes: Vec<u64>,
    consecutive_same: u32,
    consecutive_errors: u32,
}

impl StuckDetector {
    pub(crate) fn new(max_stuck_turns: u32) -> Self {
        Self {
            max_stuck_turns,
            result_hashes: Vec::new(),
            consecutive_same: 0,
            consecutive_errors: 0,
        }
    }

    /// Record a turn's tool calls + results and check for stuck patterns.
    ///
    /// Hashes both the calls (name + input) and the results (content + is_error)
    /// so that different edits to the same file (which produce identical "edited
    /// {path}" results) are correctly seen as distinct turns.
    ///
    /// Returns `Some(reason)` if the agent appears stuck, `None` otherwise.
    pub(crate) fn record_turn(
        &mut self,
        calls: &[ToolCall],
        results: &[ToolCallResult],
    ) -> Option<StuckReason> {
        // Hash both calls and results for this turn.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for c in calls {
            c.name.hash(&mut hasher);
            c.input.to_string().hash(&mut hasher);
        }
        for r in results {
            r.content.hash(&mut hasher);
            r.is_error.hash(&mut hasher);
        }
        let hash = hasher.finish();

        // Track consecutive identical results. Two identical turns back-to-back
        // means the agent sent the same calls, got the same results, and is
        // about to loop — bail immediately.
        if self.result_hashes.last() == Some(&hash) {
            self.consecutive_same += 1;
        } else {
            self.consecutive_same = 1;
        }
        self.result_hashes.push(hash);

        if self.consecutive_same >= 2 {
            return Some(StuckReason::RepeatedResults);
        }

        // Check if ALL results in this turn are errors.
        if !results.is_empty() && results.iter().all(|r| r.is_error) {
            self.consecutive_errors += 1;
            if self.consecutive_errors >= self.max_stuck_turns {
                return Some(StuckReason::ConsecutiveErrors);
            }
        } else {
            self.consecutive_errors = 0;
        }

        None
    }
}

/// Parameters for the tool-use execution loop.
pub(crate) struct ToolLoopParams<'a> {
    pub tool_defs: &'a [ToolDefinition],
    pub dispatcher: &'a ToolDispatcher,
    pub max_turns: u32,
    pub temperature: f32,
    pub max_tokens: u32,
    /// Approximate token budget for the message history. When exceeded,
    /// old `ToolResult` content is replaced with one-line summaries to
    /// stay within budget. Set to `usize::MAX` to disable pruning.
    pub context_token_budget: usize,
    /// Maximum recent turns to check for stuck detection.
    pub max_stuck_turns: u32,
}

/// Estimate the total token count of a message list using the ~4 chars/token
/// heuristic. Counts all text, tool-use JSON, and tool-result content.
pub(crate) fn estimate_message_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| match &m.content {
            MessageContent::Text(s) => s.len(),
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => text.len(),
                    ContentBlock::ToolUse(tc) => tc.input.to_string().len() + tc.name.len(),
                    ContentBlock::ToolResult(tr) => tr.content.len(),
                })
                .sum(),
        })
        .sum::<usize>()
        .div_ceil(4)
}

/// Prune old tool-result content when the message history exceeds `budget` tokens.
///
/// Strategy:
/// - Preserve the first 2 messages (system prompt + initial user message) and
///   the last 4 messages (most recent 2 turn-pairs) untouched.
/// - Never modify assistant `ToolUse` blocks (API requires paired IDs).
/// - Replace old `ToolResult` content with a one-line summary.
pub(crate) fn prune_messages(messages: &mut [Message], budget: usize) {
    if estimate_message_tokens(messages) <= budget {
        return;
    }

    let len = messages.len();
    // Preserve first 2 (system + user) and last 4 (2 turn-pairs).
    let protect_head = 2.min(len);
    let protect_tail = 4.min(len);
    let protect_tail_start = len.saturating_sub(protect_tail);

    for msg in &mut messages[protect_head..protect_tail_start.max(protect_head)] {
        if let MessageContent::Blocks(blocks) = &mut msg.content {
            for block in blocks.iter_mut() {
                if let ContentBlock::ToolResult(result) = block {
                    let first_line = result
                        .content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(80)
                        .collect::<String>();
                    let original_len = result.content.len();
                    let status = if result.is_error { "err" } else { "ok" };
                    result.content = format!("[{status}: {first_line}... ({original_len} chars)]");
                }
            }
        }
    }
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
) -> error::Result<ToolLoopOutcome> {
    let mut turns = 0u32;
    let mut detector = StuckDetector::new(params.max_stuck_turns);

    loop {
        prune_messages(messages, params.context_token_budget);

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
            return Ok(ToolLoopOutcome::Completed(response));
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

        // Execute each tool call and collect results.
        let mut results = Vec::with_capacity(response.tool_calls.len());
        for call in &response.tool_calls {
            let result = params.dispatcher.dispatch(call).await;
            if result.is_error {
                warn!(
                    role,
                    tool = call.name.as_str(),
                    error = result.content.lines().next().unwrap_or(""),
                    "tool call returned error"
                );
            } else {
                info!(role, tool = call.name.as_str(), "tool call succeeded");
            }
            results.push(result);
        }

        // Bail immediately on boundary violations — no point retrying.
        let has_boundary_violation = results
            .iter()
            .any(|r| r.is_error && r.content.contains("PROTECTED PATH"));
        if has_boundary_violation {
            warn!(
                role,
                "tool-use loop hit boundary violation, stopping immediately"
            );
            for result in results {
                messages.push(Message {
                    role: MessageRole::User,
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult(result)]),
                });
            }
            return Ok(ToolLoopOutcome::Stuck {
                reason: StuckReason::BoundaryViolation,
                last_response: response,
            });
        }

        // Check for stuck agent before appending results.
        let stuck = detector.record_turn(&response.tool_calls, &results);

        // Append results as user messages.
        for result in results {
            messages.push(Message {
                role: MessageRole::User,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult(result)]),
            });
        }

        if let Some(reason) = stuck {
            warn!(role, ?reason, "tool-use loop detected stuck agent");
            return Ok(ToolLoopOutcome::Stuck {
                reason,
                last_response: response,
            });
        }

        turns += 1;

        // Soft budget signal: warn the LLM to wrap up when >80% consumed.
        if let Ok(summary) = router.budget_summary_result().await {
            let used = summary.total_tokens;
            let limit = used + summary.tokens_remaining;
            if limit > 0 && used * 100 / limit > 80 {
                messages.push(Message {
                    role: MessageRole::User,
                    content: MessageContent::Text(
                        "[SYSTEM] Budget >80% consumed. Finish current edits and return your JSON response now. Do not start new work.".into(),
                    ),
                });
            }
        }

        if turns >= params.max_turns {
            warn!(
                role,
                turns,
                max_turns = params.max_turns,
                "tool-use loop hit max turns, returning last response"
            );
            return Ok(ToolLoopOutcome::Completed(response));
        }
    }
}

/// Render an `ObstacleKind` as a human-readable one-liner.
pub(crate) fn render_obstacle_kind(obstacle: &ObstacleKind) -> String {
    match obstacle {
        ObstacleKind::MissingPrerequisite { task_id, reason } => {
            format!("Missing prerequisite: task `{task_id}` — {reason}")
        }
        ObstacleKind::ArchitecturalGap { description } => {
            format!("Architectural gap: {description}")
        }
        ObstacleKind::ModelLimitation { model, error_class } => {
            format!("Model limitation ({model}): {error_class}")
        }
        ObstacleKind::ExternalDependency { service } => {
            format!("External dependency unavailable: {service}")
        }
        ObstacleKind::ScopeTooLarge {
            estimated_files,
            max_files,
        } => {
            format!("Scope too large: {estimated_files} files (max {max_files})")
        }
        ObstacleKind::TestFailure {
            attempts,
            last_error,
        } => {
            format!("Test failure after {attempts} attempts: {last_error}")
        }
        ObstacleKind::ParseFailure { model, raw_snippet } => {
            format!("Parse failure ({model}): {raw_snippet}")
        }
        ObstacleKind::Unknown { detail } => {
            format!("Unknown obstacle: {detail}")
        }
    }
}

/// Render a list of previous attempts as a numbered markdown section.
///
/// Each attempt is rendered with its approach, obstacle, discoveries, and
/// recommendation so that subsequent pipeline runs can learn from failures.
pub(crate) fn render_previous_attempts(attempts: &[OutcomeContext]) -> String {
    let mut out = String::new();
    for (i, attempt) in attempts.iter().enumerate() {
        out.push_str(&format!("### Attempt {}\n", i + 1));
        out.push_str(&format!("- **Approach:** {}\n", attempt.approach));
        out.push_str(&format!(
            "- **Obstacle:** {}\n",
            render_obstacle_kind(&attempt.obstacle)
        ));
        if !attempt.discoveries.is_empty() {
            out.push_str("- **Discoveries:**\n");
            for d in &attempt.discoveries {
                out.push_str(&format!("  - {d}\n"));
            }
        }
        if let Some(ref rec) = attempt.recommendation {
            out.push_str(&format!("- **Recommendation:** {rec}\n"));
        }
        out.push('\n');
    }
    out
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

    // Rust module map (injected by pipeline for implementer context).
    if let Some(serde_json::Value::String(mm)) = ctx.extra.get("module_map")
        && !mm.is_empty()
    {
        msg.push_str("\n\n");
        msg.push_str(mm);
    }

    // Failure history (stored separately so ContextAssembler can drop it independently).
    if let Some(serde_json::Value::String(fh)) = ctx.extra.get("failure_history")
        && !fh.is_empty()
    {
        msg.push_str("\n\n## Failure History\n\n");
        msg.push_str(fh);
    }

    // Structured previous attempts (injected by the orchestrator via AttemptTracker).
    if let Some(attempts_val) = ctx.extra.get("previous_attempts")
        && let Ok(attempts) = serde_json::from_value::<Vec<OutcomeContext>>(attempts_val.clone())
        && !attempts.is_empty()
    {
        msg.push_str("\n\n## Previous Attempts\n\n");
        msg.push_str(
            "The following previous attempts failed. \
             Do NOT repeat the same approach — use a different strategy.\n\n",
        );
        msg.push_str(&render_previous_attempts(&attempts));
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
                    request_id: String::new(), // Set by router
                    content: r#"{"result": "ok", "steps": [{"step_number": 1, "description": "add feature", "files": ["src/new.rs"], "action": "create"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "trivial", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false, "verdict": "pass", "issues": [], "version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": [], "diagnosis": "none", "root_cause": "none", "files_changed": ["src/new.rs"], "confidence": "high", "should_retry": false, "notes": null, "tests_added": [], "commit_message": "feat: add greet function", "summary": "test", "adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#.into(),
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
                    request_id: String::new(), // Set by router
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
            ("architect_triage".to_string(), "seq/test".to_string()),
            ("architect_review".to_string(), "seq/test".to_string()),
            ("ops_diagnosis".to_string(), "seq/test".to_string()),
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
            request_id: String::new(), // Set by router
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
            request_id: String::new(), // Set by router
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
            ("architect_triage".to_string(), "mock/test".to_string()),
            ("architect_review".to_string(), "mock/test".to_string()),
            ("ops_diagnosis".to_string(), "mock/test".to_string()),
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

    // --- previous attempts / negative context injection tests ---

    use glitchlab_kernel::outcome::{ObstacleKind, OutcomeContext};

    #[test]
    fn build_user_message_with_previous_attempts() {
        let mut ctx = base_ctx();
        let attempts = vec![OutcomeContext {
            approach: "tried adding uuid crate".into(),
            obstacle: ObstacleKind::TestFailure {
                attempts: 2,
                last_error: "compilation error".into(),
            },
            discoveries: vec!["workspace uses no_std".into()],
            recommendation: Some("use uuid without default features".into()),
            files_explored: vec!["Cargo.toml".into()],
        }];
        ctx.extra.insert(
            "previous_attempts".into(),
            serde_json::to_value(&attempts).unwrap(),
        );
        let msg = build_user_message(&ctx);
        assert!(msg.contains("## Previous Attempts"));
        assert!(msg.contains("Do NOT repeat the same approach"));
        assert!(msg.contains("### Attempt 1"));
        assert!(msg.contains("tried adding uuid crate"));
        assert!(msg.contains("Test failure after 2 attempts"));
        assert!(msg.contains("workspace uses no_std"));
        assert!(msg.contains("use uuid without default features"));
    }

    #[test]
    fn build_user_message_empty_previous_attempts_omitted() {
        let mut ctx = base_ctx();
        let attempts: Vec<OutcomeContext> = vec![];
        ctx.extra.insert(
            "previous_attempts".into(),
            serde_json::to_value(&attempts).unwrap(),
        );
        let msg = build_user_message(&ctx);
        assert!(!msg.contains("## Previous Attempts"));
    }

    #[test]
    fn build_user_message_previous_attempts_ordering() {
        let mut ctx = base_ctx();
        let attempts = vec![
            OutcomeContext {
                approach: "first approach".into(),
                obstacle: ObstacleKind::Unknown {
                    detail: "unknown".into(),
                },
                discoveries: vec![],
                recommendation: None,
                files_explored: vec![],
            },
            OutcomeContext {
                approach: "second approach".into(),
                obstacle: ObstacleKind::ArchitecturalGap {
                    description: "missing trait".into(),
                },
                discoveries: vec![],
                recommendation: None,
                files_explored: vec![],
            },
        ];
        ctx.extra.insert(
            "previous_attempts".into(),
            serde_json::to_value(&attempts).unwrap(),
        );
        let msg = build_user_message(&ctx);
        let pos1 = msg.find("### Attempt 1").unwrap();
        let pos2 = msg.find("### Attempt 2").unwrap();
        assert!(pos1 < pos2);
        assert!(msg.contains("first approach"));
        assert!(msg.contains("second approach"));
    }

    #[test]
    fn build_user_message_previous_attempts_minimal_entry() {
        let mut ctx = base_ctx();
        let attempts = vec![OutcomeContext {
            approach: "tried it".into(),
            obstacle: ObstacleKind::Unknown {
                detail: "shrug".into(),
            },
            discoveries: vec![],
            recommendation: None,
            files_explored: vec![],
        }];
        ctx.extra.insert(
            "previous_attempts".into(),
            serde_json::to_value(&attempts).unwrap(),
        );
        let msg = build_user_message(&ctx);
        assert!(msg.contains("tried it"));
        assert!(msg.contains("Unknown obstacle: shrug"));
        // No discoveries or recommendation sections.
        assert!(!msg.contains("Discoveries"));
        assert!(!msg.contains("Recommendation"));
    }

    #[test]
    fn render_obstacle_kind_all_variants() {
        let cases: Vec<(ObstacleKind, &str)> = vec![
            (
                ObstacleKind::MissingPrerequisite {
                    task_id: "t1".into(),
                    reason: "not ready".into(),
                },
                "Missing prerequisite: task `t1`",
            ),
            (
                ObstacleKind::ArchitecturalGap {
                    description: "no plugin".into(),
                },
                "Architectural gap: no plugin",
            ),
            (
                ObstacleKind::ModelLimitation {
                    model: "gpt-4".into(),
                    error_class: "overflow".into(),
                },
                "Model limitation (gpt-4): overflow",
            ),
            (
                ObstacleKind::ExternalDependency {
                    service: "npm".into(),
                },
                "External dependency unavailable: npm",
            ),
            (
                ObstacleKind::ScopeTooLarge {
                    estimated_files: 50,
                    max_files: 20,
                },
                "Scope too large: 50 files (max 20)",
            ),
            (
                ObstacleKind::TestFailure {
                    attempts: 3,
                    last_error: "assert failed".into(),
                },
                "Test failure after 3 attempts: assert failed",
            ),
            (
                ObstacleKind::ParseFailure {
                    model: "claude".into(),
                    raw_snippet: "{bad".into(),
                },
                "Parse failure (claude): {bad",
            ),
            (
                ObstacleKind::Unknown {
                    detail: "mystery".into(),
                },
                "Unknown obstacle: mystery",
            ),
        ];
        for (obstacle, expected_substr) in cases {
            let rendered = render_obstacle_kind(&obstacle);
            assert!(
                rendered.contains(expected_substr),
                "expected '{expected_substr}' in '{rendered}'"
            );
        }
    }

    #[test]
    fn render_previous_attempts_multiple_entries() {
        let attempts = vec![
            OutcomeContext {
                approach: "approach A".into(),
                obstacle: ObstacleKind::TestFailure {
                    attempts: 1,
                    last_error: "err1".into(),
                },
                discoveries: vec!["found X".into(), "found Y".into()],
                recommendation: Some("try Z".into()),
                files_explored: vec![],
            },
            OutcomeContext {
                approach: "approach B".into(),
                obstacle: ObstacleKind::Unknown {
                    detail: "dunno".into(),
                },
                discoveries: vec![],
                recommendation: None,
                files_explored: vec![],
            },
        ];
        let rendered = render_previous_attempts(&attempts);
        assert!(rendered.contains("### Attempt 1"));
        assert!(rendered.contains("### Attempt 2"));
        assert!(rendered.contains("approach A"));
        assert!(rendered.contains("approach B"));
        assert!(rendered.contains("found X"));
        assert!(rendered.contains("found Y"));
        assert!(rendered.contains("try Z"));
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
            context_token_budget: usize::MAX,
            max_stuck_turns: 3,
        };
        let outcome = tool_use_loop(&router, "implementer", &mut messages, &params)
            .await
            .unwrap();
        let ToolLoopOutcome::Completed(resp) = outcome else {
            panic!("expected Completed");
        };

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
            context_token_budget: usize::MAX,
            max_stuck_turns: 3,
        };
        let outcome = tool_use_loop(&router, "implementer", &mut messages, &params)
            .await
            .unwrap();
        let ToolLoopOutcome::Completed(resp) = outcome else {
            panic!("expected Completed");
        };

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
        let dispatcher = test_dispatcher(dir.path());

        // Each turn reads a different file so stuck detection doesn't fire.
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), format!("data-{i}")).unwrap();
        }
        let responses: Vec<RouterResponse> = (0..5)
            .map(|i| {
                tool_response(vec![tool_call(
                    &format!("call_{i}"),
                    "read_file",
                    serde_json::json!({"path": format!("f{i}.txt")}),
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
            context_token_budget: usize::MAX,
            max_stuck_turns: 10, // High enough to not trigger
        };
        let outcome = tool_use_loop(&router, "implementer", &mut messages, &params)
            .await
            .unwrap();
        let ToolLoopOutcome::Completed(resp) = outcome else {
            panic!("expected Completed, got Stuck");
        };

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
            context_token_budget: usize::MAX,
            max_stuck_turns: 3,
        };
        let outcome = tool_use_loop(&router, "implementer", &mut messages, &params)
            .await
            .unwrap();
        let ToolLoopOutcome::Completed(resp) = outcome else {
            panic!("expected Completed");
        };

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
            context_token_budget: usize::MAX,
            max_stuck_turns: 3,
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

    // --- estimate_message_tokens tests ---

    #[test]
    fn estimate_message_tokens_text() {
        let messages = vec![Message {
            role: MessageRole::User,
            content: MessageContent::Text("x".repeat(400)),
        }];
        let tokens = estimate_message_tokens(&messages);
        assert_eq!(tokens, 100); // 400 chars / 4
    }

    #[test]
    fn estimate_message_tokens_blocks() {
        use glitchlab_kernel::tool::ToolCallResult;

        let messages = vec![Message {
            role: MessageRole::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult(ToolCallResult {
                tool_call_id: "c1".into(),
                content: "x".repeat(800),
                is_error: false,
            })]),
        }];
        let tokens = estimate_message_tokens(&messages);
        assert_eq!(tokens, 200); // 800 chars / 4
    }

    // --- prune_messages tests ---

    fn make_tool_result_msg(id: &str, content: &str, is_error: bool) -> Message {
        use glitchlab_kernel::tool::ToolCallResult;
        Message {
            role: MessageRole::User,
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult(ToolCallResult {
                tool_call_id: id.into(),
                content: content.into(),
                is_error,
            })]),
        }
    }

    fn make_assistant_tool_use_msg(id: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse(
                glitchlab_kernel::tool::ToolCall {
                    id: id.into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "f.txt"}),
                },
            )]),
        }
    }

    #[test]
    fn prune_messages_under_budget_noop() {
        let mut messages = vec![
            Message {
                role: MessageRole::System,
                content: MessageContent::Text("system".into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("task".into()),
            },
            make_assistant_tool_use_msg("c1"),
            make_tool_result_msg("c1", "short result", false),
        ];
        let original = messages.clone();
        prune_messages(&mut messages, usize::MAX);
        // Nothing should change.
        assert_eq!(messages.len(), original.len());
        if let MessageContent::Blocks(blocks) = &messages[3].content
            && let ContentBlock::ToolResult(r) = &blocks[0]
        {
            assert_eq!(r.content, "short result");
        }
    }

    #[test]
    fn prune_messages_compresses_old_results() {
        let big_content = "x".repeat(4000);
        let mut messages = vec![
            // Index 0: system (protected head)
            Message {
                role: MessageRole::System,
                content: MessageContent::Text("system".into()),
            },
            // Index 1: user (protected head)
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("task".into()),
            },
            // Index 2: assistant tool use (middle — eligible but is ToolUse, not ToolResult)
            make_assistant_tool_use_msg("c1"),
            // Index 3: tool result (middle — should be compressed)
            make_tool_result_msg("c1", &big_content, false),
            // Index 4: assistant tool use (middle)
            make_assistant_tool_use_msg("c2"),
            // Index 5: tool result (middle — should be compressed)
            make_tool_result_msg("c2", &big_content, false),
            // Last 4 (protected tail):
            // Index 6: assistant tool use
            make_assistant_tool_use_msg("c3"),
            // Index 7: tool result
            make_tool_result_msg("c3", &big_content, false),
            // Index 8: assistant tool use
            make_assistant_tool_use_msg("c4"),
            // Index 9: tool result
            make_tool_result_msg("c4", "final result", false),
        ];

        // Budget is tiny so pruning must fire.
        prune_messages(&mut messages, 100);

        // Middle tool results (indices 3, 5) should be compressed.
        if let MessageContent::Blocks(blocks) = &messages[3].content
            && let ContentBlock::ToolResult(r) = &blocks[0]
        {
            assert!(
                r.content.starts_with("[ok:"),
                "expected compressed, got: {}",
                r.content
            );
            assert!(r.content.contains("chars)]"));
            assert_eq!(r.tool_call_id, "c1"); // ID preserved
        }
        if let MessageContent::Blocks(blocks) = &messages[5].content
            && let ContentBlock::ToolResult(r) = &blocks[0]
        {
            assert!(r.content.starts_with("[ok:"));
        }

        // Tail tool results (indices 7, 9) should be untouched.
        if let MessageContent::Blocks(blocks) = &messages[7].content
            && let ContentBlock::ToolResult(r) = &blocks[0]
        {
            assert_eq!(r.content, big_content);
        }
        if let MessageContent::Blocks(blocks) = &messages[9].content
            && let ContentBlock::ToolResult(r) = &blocks[0]
        {
            assert_eq!(r.content, "final result");
        }

        // Head messages should be untouched.
        assert_eq!(messages[0].content.text(), "system");
        assert_eq!(messages[1].content.text(), "task");

        // Assistant ToolUse blocks should be untouched.
        if let MessageContent::Blocks(blocks) = &messages[2].content {
            assert!(matches!(&blocks[0], ContentBlock::ToolUse(tc) if tc.id == "c1"));
        }
    }

    #[test]
    fn prune_messages_preserves_error_status() {
        let mut messages = vec![
            Message {
                role: MessageRole::System,
                content: MessageContent::Text("s".into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("t".into()),
            },
            make_assistant_tool_use_msg("c1"),
            make_tool_result_msg("c1", &"e".repeat(2000), true),
            // Tail (last 4):
            make_assistant_tool_use_msg("c2"),
            make_tool_result_msg("c2", "ok", false),
            make_assistant_tool_use_msg("c3"),
            make_tool_result_msg("c3", "ok", false),
        ];

        prune_messages(&mut messages, 10);

        if let MessageContent::Blocks(blocks) = &messages[3].content
            && let ContentBlock::ToolResult(r) = &blocks[0]
        {
            assert!(
                r.content.starts_with("[err:"),
                "error status should be preserved"
            );
        }
    }

    // --- StuckDetector tests ---

    use glitchlab_kernel::tool::ToolCallResult;
    use serde_json::json;

    fn tc(name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            input,
        }
    }

    fn tcr(content: &str, is_error: bool) -> ToolCallResult {
        ToolCallResult {
            tool_call_id: "t1".into(),
            content: content.into(),
            is_error,
        }
    }

    #[test]
    fn stuck_detector_no_stuck_on_varied_results() {
        let mut detector = StuckDetector::new(3);
        let calls = [tc("read_file", json!({"path": "a.rs"}))];
        assert_eq!(detector.record_turn(&calls, &[tcr("aaa", false)]), None);
        let calls = [tc("read_file", json!({"path": "b.rs"}))];
        assert_eq!(detector.record_turn(&calls, &[tcr("bbb", false)]), None);
        let calls = [tc("read_file", json!({"path": "c.rs"}))];
        assert_eq!(detector.record_turn(&calls, &[tcr("ccc", false)]), None);
    }

    #[test]
    fn stuck_detector_same_calls_same_results_triggers() {
        let mut detector = StuckDetector::new(3);
        let calls = [tc("read_file", json!({"path": "f.rs"}))];
        assert_eq!(detector.record_turn(&calls, &[tcr("same", false)]), None);
        // Identical call + identical result → stuck.
        assert_eq!(
            detector.record_turn(&calls, &[tcr("same", false)]),
            Some(StuckReason::RepeatedResults)
        );
    }

    #[test]
    fn stuck_detector_different_calls_same_results_not_stuck() {
        let mut detector = StuckDetector::new(3);
        // Two different edits to the same file both return "edited foo.rs".
        let calls1 = [tc(
            "edit_file",
            json!({"path": "foo.rs", "old_string": "aaa", "new_string": "bbb"}),
        )];
        let calls2 = [tc(
            "edit_file",
            json!({"path": "foo.rs", "old_string": "ccc", "new_string": "ddd"}),
        )];
        assert_eq!(
            detector.record_turn(&calls1, &[tcr("edited foo.rs", false)]),
            None
        );
        // Different inputs → different hash, not stuck.
        assert_eq!(
            detector.record_turn(&calls2, &[tcr("edited foo.rs", false)]),
            None
        );
    }

    #[test]
    fn stuck_detector_interleaved_results_not_stuck() {
        let mut detector = StuckDetector::new(3);
        let calls_a = [tc("read_file", json!({"path": "a.rs"}))];
        let calls_b = [tc(
            "edit_file",
            json!({"path": "a.rs", "old_string": "x", "new_string": "y"}),
        )];
        // A, B, A pattern is normal (read → edit → read), not stuck.
        assert_eq!(detector.record_turn(&calls_a, &[tcr("aaa", false)]), None);
        assert_eq!(
            detector.record_turn(&calls_b, &[tcr("edited a.rs", false)]),
            None
        );
        assert_eq!(detector.record_turn(&calls_a, &[tcr("aaa", false)]), None);
        assert_eq!(
            detector.record_turn(&calls_b, &[tcr("edited a.rs", false)]),
            None
        );
    }

    #[test]
    fn stuck_detector_consecutive_errors() {
        let mut detector = StuckDetector::new(3);
        // Each turn has different content to avoid triggering RepeatedResults.
        let c1 = [tc("run_command", json!({"command": "test1"}))];
        let c2 = [tc("run_command", json!({"command": "test2"}))];
        let c3 = [tc("run_command", json!({"command": "test3"}))];
        assert_eq!(detector.record_turn(&c1, &[tcr("err1", true)]), None);
        assert_eq!(detector.record_turn(&c2, &[tcr("err2", true)]), None);
        assert_eq!(
            detector.record_turn(&c3, &[tcr("err3", true)]),
            Some(StuckReason::ConsecutiveErrors)
        );
    }

    #[test]
    fn stuck_detector_error_streak_resets() {
        let mut detector = StuckDetector::new(3);
        let c1 = [tc("run_command", json!({"command": "a"}))];
        let c2 = [tc("read_file", json!({"path": "ok"}))];
        let c3 = [tc("run_command", json!({"command": "b"}))];
        let c4 = [tc("run_command", json!({"command": "c"}))];
        assert_eq!(detector.record_turn(&c1, &[tcr("err1", true)]), None);
        assert_eq!(detector.record_turn(&c2, &[tcr("ok1", false)]), None); // resets
        assert_eq!(detector.record_turn(&c3, &[tcr("err2", true)]), None);
        assert_eq!(detector.record_turn(&c4, &[tcr("err3", true)]), None);
        // Only 2 consecutive errors, not 3 — no trigger.
    }

    #[test]
    fn stuck_detector_mixed_results_not_stuck() {
        let mut detector = StuckDetector::new(3);
        // Turns with some errors and some successes — should NOT trigger consecutive errors.
        let c1 = [
            tc("run_command", json!({"command": "a"})),
            tc("read_file", json!({"path": "b"})),
        ];
        let c2 = [
            tc("run_command", json!({"command": "c"})),
            tc("read_file", json!({"path": "d"})),
        ];
        let c3 = [
            tc("run_command", json!({"command": "e"})),
            tc("read_file", json!({"path": "f"})),
        ];
        let mixed1 = vec![tcr("err", true), tcr("ok", false)];
        let mixed2 = vec![tcr("err2", true), tcr("ok2", false)];
        let mixed3 = vec![tcr("err3", true), tcr("ok3", false)];
        assert_eq!(detector.record_turn(&c1, &mixed1), None);
        assert_eq!(detector.record_turn(&c2, &mixed2), None);
        assert_eq!(detector.record_turn(&c3, &mixed3), None);
    }

    #[tokio::test]
    async fn tool_use_loop_returns_stuck_on_repeat() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "same data").unwrap();
        let dispatcher = test_dispatcher(dir.path());

        // 5 identical tool calls reading the same file.
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
            content: MessageContent::Text("read f.txt".into()),
        }];

        let params = ToolLoopParams {
            tool_defs: &tool_defs,
            dispatcher: &dispatcher,
            max_turns: 20,
            temperature: 0.2,
            max_tokens: 4096,
            context_token_budget: usize::MAX,
            max_stuck_turns: 3,
        };
        let outcome = tool_use_loop(&router, "implementer", &mut messages, &params)
            .await
            .unwrap();

        assert!(
            matches!(
                outcome,
                ToolLoopOutcome::Stuck {
                    reason: StuckReason::RepeatedResults,
                    ..
                }
            ),
            "expected Stuck(RepeatedResults), got {outcome:?}"
        );
    }

    #[test]
    fn prune_messages_empty() {
        let mut messages: Vec<Message> = vec![];
        prune_messages(&mut messages, 10);
        assert!(messages.is_empty());
    }

    #[test]
    fn prune_messages_single_message() {
        let mut messages = vec![Message {
            role: MessageRole::System,
            content: MessageContent::Text("system prompt".into()),
        }];
        prune_messages(&mut messages, 10);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn prune_messages_two_messages() {
        let mut messages = vec![
            Message {
                role: MessageRole::System,
                content: MessageContent::Text("system prompt".into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("user message".into()),
            },
        ];
        prune_messages(&mut messages, 10);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn build_user_message_with_module_map() {
        let mut ctx = base_ctx();
        ctx.extra.insert(
            "module_map".into(),
            serde_json::Value::String(
                "## Rust Module Map\n\n### `src/lib.rs`\n- `pub mod agents;`\n".into(),
            ),
        );
        let msg = build_user_message(&ctx);
        assert!(msg.contains("## Rust Module Map"));
        assert!(msg.contains("`pub mod agents;`"));
    }

    #[test]
    fn build_user_message_empty_module_map_omitted() {
        let mut ctx = base_ctx();
        ctx.extra.insert(
            "module_map".into(),
            serde_json::Value::String(String::new()),
        );
        let msg = build_user_message(&ctx);
        assert!(!msg.contains("Module Map"));
    }
}
