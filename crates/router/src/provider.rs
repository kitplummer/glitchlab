pub mod anthropic;
pub mod openai;

use std::future::Future;
use std::pin::Pin;

use glitchlab_kernel::agent::Message;

use crate::response::RouterResponse;

/// Boxed future returned by Provider methods (for dyn compatibility).
pub type ProviderFuture<'a> =
    Pin<Box<dyn Future<Output = Result<RouterResponse, ProviderError>> + Send + 'a>>;

// ---------------------------------------------------------------------------
// Provider trait
// ---------------------------------------------------------------------------

/// A provider handles the actual HTTP call to an LLM API.
///
/// Each provider knows how to translate our generic `Message` format
/// into the provider-specific request shape and parse the response back.
pub trait Provider: Send + Sync {
    /// Make a completion call.
    fn complete(
        &self,
        model: &str,
        messages: &[Message],
        temperature: f32,
        max_tokens: u32,
        response_format: Option<&serde_json::Value>,
    ) -> ProviderFuture<'_>;
}

/// Errors from a provider.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API error ({status}): {body}")]
    Api { status: u16, body: String },

    #[error("missing API key: {0}")]
    MissingApiKey(String),

    #[error("failed to parse response: {0}")]
    Parse(String),

    #[error("rate limited, retry after {retry_after_ms:?}ms")]
    RateLimited { retry_after_ms: Option<u64> },
}

// ---------------------------------------------------------------------------
// Provider resolution from model string
// ---------------------------------------------------------------------------

/// Parse a model string like "anthropic/claude-sonnet-4-20250514" into
/// (provider_name, model_id).
pub fn parse_model_string(model: &str) -> (&str, &str) {
    match model.split_once('/') {
        Some((provider, model_id)) => (provider, model_id),
        None => ("openai", model), // Default to OpenAI-compatible if no prefix.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_anthropic_model() {
        let (provider, model) = parse_model_string("anthropic/claude-sonnet-4-20250514");
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn parse_gemini_model() {
        let (provider, model) = parse_model_string("gemini/gemini-2.5-flash-lite");
        assert_eq!(provider, "gemini");
        assert_eq!(model, "gemini-2.5-flash-lite");
    }

    #[test]
    fn parse_bare_model_defaults_to_openai() {
        let (provider, model) = parse_model_string("gpt-4o");
        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-4o");
    }
}
