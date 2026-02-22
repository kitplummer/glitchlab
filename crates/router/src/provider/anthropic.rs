use std::time::Instant;

use glitchlab_kernel::agent::{Message, MessageRole};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::{Provider, ProviderError, ProviderFuture};
use crate::response::RouterResponse;

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: ANTHROPIC_API_URL.into(),
        }
    }

    pub fn from_env() -> Result<Self, ProviderError> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| ProviderError::MissingApiKey("ANTHROPIC_API_KEY".into()))?;
        Ok(Self::new(key))
    }

    /// Custom base URL (for testing or proxying).
    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url,
        }
    }
}

impl Provider for AnthropicProvider {
    fn complete(
        &self,
        model: &str,
        messages: &[Message],
        temperature: f32,
        max_tokens: u32,
        _response_format: Option<&serde_json::Value>,
    ) -> ProviderFuture<'_> {
        let model = model.to_string();
        let messages = messages.to_vec();

        Box::pin(async move {
            // Separate system from conversation messages.
            let system: String = messages
                .iter()
                .filter(|m| m.role == MessageRole::System)
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");

            let conversation: Vec<_> = messages
                .iter()
                .filter(|m| m.role != MessageRole::System)
                .map(|m| AnthropicMessage {
                    role: match m.role {
                        MessageRole::User => "user".into(),
                        MessageRole::Assistant => "assistant".into(),
                        MessageRole::System => unreachable!(),
                    },
                    content: m.content.clone(),
                })
                .collect();

            let body = AnthropicRequest {
                model: &model,
                max_tokens,
                temperature,
                system: if system.is_empty() {
                    None
                } else {
                    Some(&system)
                },
                messages: &conversation,
            };

            let start = Instant::now();

            let resp = self
                .client
                .post(&self.base_url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await?;

            let latency_ms = start.elapsed().as_millis() as u64;
            let status = resp.status().as_u16();

            if status == 429 {
                return Err(ProviderError::RateLimited {
                    retry_after_ms: resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(|s| s * 1000),
                });
            }

            let resp_text = resp.text().await?;

            if status >= 400 {
                return Err(ProviderError::Api {
                    status,
                    body: resp_text,
                });
            }

            let parsed: AnthropicResponse = serde_json::from_str(&resp_text)
                .map_err(|e| ProviderError::Parse(format!("{e}: {resp_text}")))?;

            let content = parsed
                .content
                .into_iter()
                .filter_map(|block| {
                    if block.r#type == "text" {
                        block.text
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("");

            let prompt_tokens = parsed.usage.input_tokens;
            let completion_tokens = parsed.usage.output_tokens;
            let total_tokens = prompt_tokens + completion_tokens;
            let cost = estimate_anthropic_cost(&model, prompt_tokens, completion_tokens);

            Ok(RouterResponse {
                content,
                model: format!("anthropic/{model}"),
                prompt_tokens,
                completion_tokens,
                total_tokens,
                cost,
                latency_ms,
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Request/response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: &'a [AnthropicMessage],
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
    usage: Usage,
}

#[derive(Deserialize)]
struct ContentBlock {
    r#type: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
}

fn estimate_anthropic_cost(model: &str, prompt_tokens: u64, completion_tokens: u64) -> f64 {
    let (input_per_m, output_per_m) = if model.contains("opus") {
        (15.0, 75.0)
    } else if model.contains("sonnet") {
        (3.0, 15.0)
    } else if model.contains("haiku") {
        (0.25, 1.25)
    } else {
        (3.0, 15.0)
    };

    let input_cost = (prompt_tokens as f64 / 1_000_000.0) * input_per_m;
    let output_cost = (completion_tokens as f64 / 1_000_000.0) * output_per_m;
    input_cost + output_cost
}

#[cfg(test)]
mod tests {
    use super::*;
    use glitchlab_kernel::agent::{Message, MessageRole};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn mock_server(status: u16, body: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let status_line = match status {
            200 => "200 OK",
            429 => "429 Too Many Requests",
            _ => "500 Internal Server Error",
        };
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = stream.read(&mut buf).await.unwrap();
            let resp = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
        });
        url
    }

    fn test_messages() -> Vec<Message> {
        vec![
            Message { role: MessageRole::System, content: "System prompt".into() },
            Message { role: MessageRole::User, content: "Hello".into() },
        ]
    }

    #[test]
    fn provider_construction() {
        let _provider = AnthropicProvider::new("test-key".into());
    }

    #[test]
    fn provider_with_base_url() {
        let p = AnthropicProvider::with_base_url("key".into(), "http://localhost:1234".into());
        assert_eq!(p.base_url, "http://localhost:1234");
    }

    #[tokio::test]
    async fn complete_success() {
        let body = serde_json::json!({
            "content": [{"type": "text", "text": "{\"result\": \"ok\"}"}],
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = AnthropicProvider::with_base_url("test-key".into(), url);
        let result = provider.complete("claude-sonnet-4-20250514", &test_messages(), 0.2, 4096, None).await;
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.content.contains("result"));
        assert_eq!(response.prompt_tokens, 100);
        assert_eq!(response.completion_tokens, 50);
        assert_eq!(response.total_tokens, 150);
        assert!(response.model.contains("anthropic"));
    }

    #[tokio::test]
    async fn complete_rate_limited() {
        let url = mock_server(429, "{}".into()).await;
        let provider = AnthropicProvider::with_base_url("test-key".into(), url);
        let result = provider.complete("claude-sonnet-4-20250514", &test_messages(), 0.2, 4096, None).await;
        assert!(matches!(result, Err(ProviderError::RateLimited { .. })));
    }

    #[tokio::test]
    async fn complete_api_error() {
        let url = mock_server(500, r#"{"error": "internal"}"#.into()).await;
        let provider = AnthropicProvider::with_base_url("test-key".into(), url);
        let result = provider.complete("claude-sonnet-4-20250514", &test_messages(), 0.2, 4096, None).await;
        assert!(matches!(result, Err(ProviderError::Api { .. })));
    }

    #[tokio::test]
    async fn complete_system_only_messages() {
        let body = serde_json::json!({
            "content": [{"type": "text", "text": "hello"}],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = AnthropicProvider::with_base_url("test-key".into(), url);
        let messages = vec![
            Message { role: MessageRole::User, content: "Hi".into() },
        ];
        let result = provider.complete("claude-haiku-3-20240307", &messages, 0.5, 1024, None).await;
        assert!(result.is_ok());
    }

    #[test]
    fn cost_opus() {
        let cost = estimate_anthropic_cost("claude-opus-4-20250514", 1_000_000, 1_000_000);
        assert!((cost - 90.0).abs() < 0.01);
    }

    #[test]
    fn cost_sonnet() {
        let cost = estimate_anthropic_cost("claude-sonnet-4-20250514", 1_000_000, 1_000_000);
        assert!((cost - 18.0).abs() < 0.01);
    }

    #[test]
    fn cost_haiku() {
        let cost = estimate_anthropic_cost("claude-haiku-3-20240307", 1_000_000, 1_000_000);
        assert!((cost - 1.5).abs() < 0.01);
    }

    #[test]
    fn cost_unknown_defaults_to_sonnet() {
        let cost = estimate_anthropic_cost("unknown-model", 1_000_000, 1_000_000);
        assert!((cost - 18.0).abs() < 0.01);
    }

    #[test]
    fn cost_zero_tokens() {
        let cost = estimate_anthropic_cost("claude-sonnet-4-20250514", 0, 0);
        assert!((cost - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cost_small_usage() {
        let cost = estimate_anthropic_cost("claude-sonnet-4-20250514", 1000, 500);
        let expected = (1000.0 / 1_000_000.0) * 3.0 + (500.0 / 1_000_000.0) * 15.0;
        assert!((cost - expected).abs() < 0.0001);
    }
}
