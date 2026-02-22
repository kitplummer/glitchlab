use serde::{Deserialize, Serialize};

/// Response from an LLM completion call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterResponse {
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
}
