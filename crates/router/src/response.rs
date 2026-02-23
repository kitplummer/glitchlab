use glitchlab_kernel::tool::ToolCall;
use serde::{Deserialize, Serialize};

/// Response from an LLM completion call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterResponse {
    /// Unique request identifier (UUID v4).
    pub request_id: String,

    /// Raw text content from the LLM.
    pub content: String,

    /// Resolved model string (e.g. "anthropic/claude-sonnet-4-20250514").
    pub model: String,

    /// Prompt tokens used.
    pub prompt_tokens: u64,

    /// Completion tokens used.
    pub completion_tokens: u64,

    /// Total tokens used.
    pub total_tokens: u64,

    /// Estimated cost in USD.
    pub cost: f64,

    /// Roundtrip latency in milliseconds.
    pub latency_ms: u64,

    /// Tool calls requested by the LLM (empty for non-tool responses).
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,

    /// Why the model stopped generating: "tool_use", "end_turn", etc.
    #[serde(default)]
    pub stop_reason: Option<String>,
}
