use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error;
use crate::tool::{ToolCall, ToolCallResult};

// ---------------------------------------------------------------------------
// Agent trait
// ---------------------------------------------------------------------------

/// A domain-specific LLM-powered worker.
///
/// Agents are stateless between invocations. All state lives in the
/// `AgentContext` passed to `execute` and the `AgentOutput` returned.
pub trait Agent: Send + Sync {
    /// Unique identifier within the org (e.g. "planner", "implementer").
    fn role(&self) -> &str;

    /// Human-readable persona name (e.g. "Professor Zap", "Patch").
    fn persona(&self) -> &str;

    /// Execute this agent's task.
    ///
    /// The agent builds a prompt from `ctx`, calls an LLM via the router,
    /// parses the structured response, and returns the result.
    fn execute(
        &self,
        ctx: &AgentContext,
    ) -> impl Future<Output = error::Result<AgentOutput>> + Send;
}

// ---------------------------------------------------------------------------
// AgentContext — everything an agent needs to do its work
// ---------------------------------------------------------------------------

/// Context passed to every agent invocation.
///
/// Mirrors the Python `AgentContext` but adds explicit budget awareness
/// and checkpoint data for Gastown-style recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContext {
    /// Unique task identifier.
    pub task_id: String,

    /// Full task description with all injected context
    /// (prelude, repo index, failure history, constraints).
    pub objective: String,

    /// Absolute path to the original repository.
    pub repo_path: String,

    /// Absolute path to the isolated worktree for this task.
    pub working_dir: String,

    /// Task-level constraints (from task definition + prelude).
    #[serde(default)]
    pub constraints: Vec<String>,

    /// Success criteria that must be met.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,

    /// Risk level: "low", "medium", "high".
    #[serde(default = "default_risk")]
    pub risk_level: String,

    /// Filename → content for files relevant to this agent.
    #[serde(default)]
    pub file_context: HashMap<String, String>,

    /// Output from the previous pipeline stage.
    #[serde(default)]
    pub previous_output: serde_json::Value,

    /// Agent-specific extra context (e.g. prelude_context, test_output).
    #[serde(default)]
    pub extra: HashMap<String, serde_json::Value>,
}

fn default_risk() -> String {
    "low".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_context_default_risk() {
        let json =
            r#"{"task_id": "t1", "objective": "test", "repo_path": "/tmp", "working_dir": "/tmp"}"#;
        let ctx: AgentContext = serde_json::from_str(json).unwrap();
        assert_eq!(ctx.risk_level, "low");
    }

    #[test]
    fn message_role_serde() {
        let role = MessageRole::System;
        let json = serde_json::to_string(&role).unwrap();
        assert_eq!(json, "\"system\"");

        let parsed: MessageRole = serde_json::from_str("\"assistant\"").unwrap();
        assert_eq!(parsed, MessageRole::Assistant);
    }

    #[test]
    fn message_role_tool_serde() {
        let role = MessageRole::Tool;
        let json = serde_json::to_string(&role).unwrap();
        assert_eq!(json, "\"tool\"");
        let parsed: MessageRole = serde_json::from_str("\"tool\"").unwrap();
        assert_eq!(parsed, MessageRole::Tool);
    }

    #[test]
    fn agent_output_parse_error_default() {
        let json = r#"{"data": {}, "metadata": {"agent": "test", "model": "m", "tokens": 0, "cost": 0.0, "latency_ms": 0}}"#;
        let output: AgentOutput = serde_json::from_str(json).unwrap();
        assert!(!output.parse_error);
    }

    #[test]
    fn agent_context_defaults() {
        let json =
            r#"{"task_id": "t1", "objective": "test", "repo_path": "/tmp", "working_dir": "/tmp"}"#;
        let ctx: AgentContext = serde_json::from_str(json).unwrap();
        assert!(ctx.file_context.is_empty());
        assert!(ctx.constraints.is_empty());
        assert!(ctx.extra.is_empty());
        assert_eq!(ctx.previous_output, serde_json::Value::Null);
    }

    #[test]
    fn agent_context_serde_roundtrip() {
        let ctx = AgentContext {
            task_id: "t1".into(),
            objective: "Do stuff".into(),
            repo_path: "/tmp".into(),
            working_dir: "/tmp/wt".into(),
            constraints: vec!["no deps".into()],
            acceptance_criteria: vec!["tests pass".into()],
            risk_level: "medium".into(),
            file_context: HashMap::from([("src/lib.rs".into(), "code".into())]),
            previous_output: serde_json::json!({"plan": "do stuff"}),
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let parsed: AgentContext = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.task_id, "t1");
        assert_eq!(parsed.constraints.len(), 1);
    }

    #[test]
    fn message_content_text_serde_bare_string() {
        let content = MessageContent::Text("hello".into());
        let json = serde_json::to_string(&content).unwrap();
        assert_eq!(json, "\"hello\"");
        let parsed: MessageContent = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_text());
        assert_eq!(parsed.text(), "hello");
    }

    #[test]
    fn message_content_blocks_serde() {
        let content = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "Here is the result:".into(),
            },
            ContentBlock::ToolUse(ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "src/lib.rs"}),
            }),
        ]);
        let json = serde_json::to_string(&content).unwrap();
        let parsed: MessageContent = serde_json::from_str(&json).unwrap();
        assert!(!parsed.is_text());
        assert_eq!(parsed.text(), "Here is the result:");
        assert_eq!(parsed.tool_calls().len(), 1);
        assert_eq!(parsed.tool_calls()[0].name, "read_file");
    }

    #[test]
    fn content_block_text_serde_roundtrip() {
        let block = ContentBlock::Text {
            text: "hello world".into(),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        match parsed {
            ContentBlock::Text { text } => assert_eq!(text, "hello world"),
            _ => panic!("expected Text block"),
        }
    }

    #[test]
    fn content_block_tool_use_serde_roundtrip() {
        let block = ContentBlock::ToolUse(ToolCall {
            id: "tc_42".into(),
            name: "run_command".into(),
            input: serde_json::json!({"command": "ls"}),
        });
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"tool_use\""));
        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        match parsed {
            ContentBlock::ToolUse(tc) => {
                assert_eq!(tc.id, "tc_42");
                assert_eq!(tc.name, "run_command");
            }
            _ => panic!("expected ToolUse block"),
        }
    }

    #[test]
    fn content_block_tool_result_serde_roundtrip() {
        let block = ContentBlock::ToolResult(ToolCallResult {
            tool_call_id: "tc_42".into(),
            content: "file contents".into(),
            is_error: false,
        });
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"tool_result\""));
        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        match parsed {
            ContentBlock::ToolResult(tr) => {
                assert_eq!(tr.tool_call_id, "tc_42");
                assert!(!tr.is_error);
            }
            _ => panic!("expected ToolResult block"),
        }
    }

    #[test]
    fn message_content_text_returns_text_for_text_variant() {
        let content = MessageContent::Text("simple text".into());
        assert_eq!(content.text(), "simple text");
    }

    #[test]
    fn message_content_text_joins_blocks() {
        let content = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "part1".into(),
            },
            ContentBlock::ToolUse(ToolCall {
                id: "c".into(),
                name: "t".into(),
                input: serde_json::Value::Null,
            }),
            ContentBlock::Text {
                text: "part2".into(),
            },
        ]);
        assert_eq!(content.text(), "part1part2");
    }

    #[test]
    fn message_content_tool_calls_empty_for_text() {
        let content = MessageContent::Text("no tools".into());
        assert!(content.tool_calls().is_empty());
    }

    #[test]
    fn message_content_tool_calls_extracts_tool_use() {
        let content = MessageContent::Blocks(vec![
            ContentBlock::Text { text: "hi".into() },
            ContentBlock::ToolUse(ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({}),
            }),
            ContentBlock::ToolUse(ToolCall {
                id: "c2".into(),
                name: "write_file".into(),
                input: serde_json::json!({}),
            }),
        ]);
        let calls = content.tool_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[1].id, "c2");
    }

    #[test]
    fn message_content_is_text_true_false() {
        assert!(MessageContent::Text("x".into()).is_text());
        assert!(!MessageContent::Blocks(vec![]).is_text());
    }

    #[test]
    fn message_backward_compat_json() {
        let msg = Message {
            role: MessageRole::User,
            content: MessageContent::Text("hello".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert_eq!(json, r#"{"role":"user","content":"hello"}"#);
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, MessageRole::User);
        assert_eq!(parsed.content.text(), "hello");
    }
}

// ---------------------------------------------------------------------------
// AgentOutput — what an agent returns
// ---------------------------------------------------------------------------

/// Structured output from an agent invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutput {
    /// Parsed structured data from the LLM response.
    /// Schema varies per agent role.
    pub data: serde_json::Value,

    /// Metadata about the LLM call that produced this output.
    pub metadata: AgentMetadata,

    /// True if the output was constructed from a fallback/default
    /// because the LLM response couldn't be parsed.
    /// This allows downstream consumers to handle parse failures gracefully.
    #[serde(default)]
    pub parse_error: bool,
}

/// Metadata tagged on every agent output for auditability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMetadata {
    /// Agent role that produced this output.
    pub agent: String,

    /// Resolved model string (e.g. "anthropic/claude-sonnet-4-20250514").
    pub model: String,

    /// Tokens consumed by this call.
    pub tokens: u64,

    /// Estimated cost in USD.
    pub cost: f64,

    /// Roundtrip latency in milliseconds.
    pub latency_ms: u64,
}

// ---------------------------------------------------------------------------
// Message types for LLM calls
// ---------------------------------------------------------------------------

/// A single content block within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text { text: String },
    /// LLM requests a tool call.
    ToolUse(ToolCall),
    /// Result of a tool execution, sent back to the LLM.
    ToolResult(ToolCallResult),
}

/// Message content: either a simple string or structured blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Simple text content (backward-compatible with existing code).
    Text(String),
    /// Structured content blocks (tool use, tool results, mixed text).
    Blocks(Vec<ContentBlock>),
}

impl MessageContent {
    /// Get the text content, joining all text blocks if structured.
    pub fn text(&self) -> String {
        match self {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }

    /// Extract tool calls from this content (empty if text-only).
    pub fn tool_calls(&self) -> Vec<&ToolCall> {
        match self {
            MessageContent::Text(_) => vec![],
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse(tc) => Some(tc),
                    _ => None,
                })
                .collect(),
        }
    }

    /// True if this is simple text with no tool blocks.
    pub fn is_text(&self) -> bool {
        matches!(self, MessageContent::Text(_))
    }
}

/// A chat message for LLM completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: MessageContent,
}

/// Standard chat roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}
