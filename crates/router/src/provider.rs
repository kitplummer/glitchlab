pub mod anthropic;
pub mod gemini;
pub mod openai;

use std::future::Future;
use std::pin::Pin;

use glitchlab_kernel::agent::Message;
use glitchlab_kernel::tool::ToolDefinition;

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

    /// Complete with tool definitions available.
    ///
    /// Default implementation ignores tools and delegates to `complete()`.
    /// Provider implementations override this in Phase 1 to marshal tool
    /// definitions into provider-specific request formats.
    fn complete_with_tools(
        &self,
        model: &str,
        messages: &[Message],
        temperature: f32,
        max_tokens: u32,
        tools: &[ToolDefinition],
        response_format: Option<&serde_json::Value>,
    ) -> ProviderFuture<'_> {
        let _ = tools;
        self.complete(model, messages, temperature, max_tokens, response_format)
    }
}

// ---------------------------------------------------------------------------
// ProviderInit — boundary type between config and router
// ---------------------------------------------------------------------------

/// Resolved provider credentials, passed from config to the router.
///
/// This is a plain struct (no serde) that acts as the boundary between
/// eng-org config parsing and router provider construction.
///
/// ## API Key Resolution Priority
///
/// The `api_key` field contains the resolved API key based on the following priority order:
/// 1. **Inline key** - Explicitly provided key in configuration
/// 2. **Custom env var** - Provider-specific environment variable (e.g., `CUSTOM_ANTHROPIC_KEY`)
/// 3. **Default env var** - Standard environment variable (e.g., `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`)
#[derive(Debug, Clone)]
pub struct ProviderInit {
    /// Provider kind: "anthropic", "gemini", "openai", or a custom name.
    pub kind: String,
    /// Resolved API key.
    pub api_key: String,
    /// Optional base URL override.
    pub base_url: Option<String>,
    /// Display/map key (defaults to kind if not set).
    pub name: Option<String>,
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

    #[test]
    fn provider_init_fields() {
        let init = ProviderInit {
            kind: "anthropic".into(),
            api_key: "sk-test".into(),
            base_url: Some("http://localhost:8080".into()),
            name: Some("my-anthropic".into()),
        };
        assert_eq!(init.kind, "anthropic");
        assert_eq!(init.api_key, "sk-test");
        assert_eq!(init.base_url.as_deref(), Some("http://localhost:8080"));
        assert_eq!(init.name.as_deref(), Some("my-anthropic"));

        // Default name is None.
        let init2 = ProviderInit {
            kind: "openai".into(),
            api_key: "key".into(),
            base_url: None,
            name: None,
        };
        assert!(init2.base_url.is_none());
        assert!(init2.name.is_none());
    }
}
