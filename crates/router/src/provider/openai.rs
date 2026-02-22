use std::time::Instant;

use glitchlab_kernel::agent::{Message, MessageRole};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::{Provider, ProviderError, ProviderFuture};
use crate::response::RouterResponse;

const OPENAI_API_URL: &str = "https://api.openai.com/v1/chat/completions";
const GEMINI_OPENAI_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions";

pub struct OpenAiProvider {
    client: Client,
    api_key: String,
    base_url: String,
    provider_name: String,
}

impl OpenAiProvider {
    pub fn new(api_key: String, base_url: String, provider_name: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url,
            provider_name,
        }
    }

    pub fn openai_from_env() -> Result<Self, ProviderError> {
        let key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| ProviderError::MissingApiKey("OPENAI_API_KEY".into()))?;
        Ok(Self::new(key, OPENAI_API_URL.into(), "openai".into()))
    }

    pub fn gemini_from_env() -> Result<Self, ProviderError> {
        let key = std::env::var("GOOGLE_API_KEY")
            .or_else(|_| std::env::var("GEMINI_API_KEY"))
            .map_err(|_| ProviderError::MissingApiKey("GOOGLE_API_KEY or GEMINI_API_KEY".into()))?;
        Ok(Self::new(key, GEMINI_OPENAI_URL.into(), "gemini".into()))
    }

    pub fn custom(api_key: String, base_url: String, name: String) -> Self {
        Self::new(api_key, base_url, name)
    }
}

impl Provider for OpenAiProvider {
    fn complete(
        &self,
        model: &str,
        messages: &[Message],
        temperature: f32,
        max_tokens: u32,
        response_format: Option<&serde_json::Value>,
    ) -> ProviderFuture<'_> {
        let model = model.to_string();
        let messages = messages.to_vec();
        let response_format = response_format.cloned();

        Box::pin(async move {
            let oai_messages: Vec<OaiMessage> = messages
                .iter()
                .map(|m| OaiMessage {
                    role: match m.role {
                        MessageRole::System => "system".into(),
                        MessageRole::User => "user".into(),
                        MessageRole::Assistant => "assistant".into(),
                    },
                    content: m.content.clone(),
                })
                .collect();

            let body = OaiRequest {
                model: &model,
                messages: &oai_messages,
                temperature,
                max_tokens,
                response_format: response_format.as_ref(),
            };

            let start = Instant::now();

            let resp = self
                .client
                .post(&self.base_url)
                .header("Authorization", format!("Bearer {}", self.api_key))
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

            let parsed: OaiResponse = serde_json::from_str(&resp_text)
                .map_err(|e| ProviderError::Parse(format!("{e}: {resp_text}")))?;

            let choice = parsed
                .choices
                .into_iter()
                .next()
                .ok_or_else(|| ProviderError::Parse("no choices in response".into()))?;

            let content = choice.message.content.unwrap_or_default();

            let prompt_tokens = parsed.usage.prompt_tokens;
            let completion_tokens = parsed.usage.completion_tokens;
            let total_tokens = parsed.usage.total_tokens;
            let cost = estimate_openai_cost(
                &self.provider_name,
                &model,
                prompt_tokens,
                completion_tokens,
            );

            Ok(RouterResponse {
                content,
                model: format!("{}/{model}", self.provider_name),
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
struct OaiRequest<'a> {
    model: &'a str,
    messages: &'a [OaiMessage],
    temperature: f32,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<&'a serde_json::Value>,
}

#[derive(Serialize)]
struct OaiMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OaiResponse {
    choices: Vec<OaiChoice>,
    usage: OaiUsage,
}

#[derive(Deserialize)]
struct OaiChoice {
    message: OaiChoiceMessage,
}

#[derive(Deserialize)]
struct OaiChoiceMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct OaiUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

fn estimate_openai_cost(
    provider: &str,
    model: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> f64 {
    let (input_per_m, output_per_m) = match provider {
        "gemini" => {
            if model.contains("flash-lite") {
                (0.075, 0.30)
            } else if model.contains("flash") {
                (0.15, 0.60)
            } else if model.contains("pro") {
                (1.25, 5.00)
            } else {
                (0.15, 0.60)
            }
        }
        _ => {
            if model.contains("gpt-4o-mini") {
                (0.15, 0.60)
            } else if model.contains("gpt-4o") {
                (2.50, 10.0)
            } else if model.contains("o1") || model.contains("o3") {
                (10.0, 40.0)
            } else {
                (2.50, 10.0)
            }
        }
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
            Message {
                role: MessageRole::System,
                content: "System prompt".into(),
            },
            Message {
                role: MessageRole::User,
                content: "Hello".into(),
            },
        ]
    }

    #[test]
    fn provider_construction() {
        let _p = OpenAiProvider::new("key".into(), "http://localhost".into(), "test".into());
    }

    #[test]
    fn custom_provider() {
        let _p = OpenAiProvider::custom(
            "key".into(),
            "http://localhost:8080/v1".into(),
            "ollama".into(),
        );
    }

    #[tokio::test]
    async fn complete_success() {
        let body = serde_json::json!({
            "choices": [{"message": {"content": "{\"result\": \"ok\"}"}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150}
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = OpenAiProvider::new("test-key".into(), url, "openai".into());
        let result = provider
            .complete("gpt-4o", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.content.contains("result"));
        assert_eq!(response.prompt_tokens, 100);
        assert_eq!(response.completion_tokens, 50);
        assert_eq!(response.total_tokens, 150);
    }

    #[tokio::test]
    async fn complete_rate_limited() {
        let url = mock_server(429, "{}".into()).await;
        let provider = OpenAiProvider::new("test-key".into(), url, "openai".into());
        let result = provider
            .complete("gpt-4o", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(matches!(result, Err(ProviderError::RateLimited { .. })));
    }

    #[tokio::test]
    async fn complete_api_error() {
        let url = mock_server(500, r#"{"error": "internal"}"#.into()).await;
        let provider = OpenAiProvider::new("test-key".into(), url, "openai".into());
        let result = provider
            .complete("gpt-4o", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(matches!(result, Err(ProviderError::Api { .. })));
    }

    #[tokio::test]
    async fn complete_gemini_provider() {
        let body = serde_json::json!({
            "choices": [{"message": {"content": "hello"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = OpenAiProvider::new("test-key".into(), url, "gemini".into());
        let result = provider
            .complete("gemini-2.5-flash", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert!(resp.model.contains("gemini"));
    }

    #[tokio::test]
    async fn complete_with_response_format() {
        let body = serde_json::json!({
            "choices": [{"message": {"content": "{}"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = OpenAiProvider::new("test-key".into(), url, "openai".into());
        let format = serde_json::json!({"type": "json_object"});
        let result = provider
            .complete("gpt-4o", &test_messages(), 0.2, 4096, Some(&format))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn complete_with_assistant_message() {
        let body = serde_json::json!({
            "choices": [{"message": {"content": "response"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = OpenAiProvider::new("test-key".into(), url, "openai".into());
        let messages = vec![
            Message {
                role: MessageRole::System,
                content: "System".into(),
            },
            Message {
                role: MessageRole::User,
                content: "Hi".into(),
            },
            Message {
                role: MessageRole::Assistant,
                content: "Hello".into(),
            },
            Message {
                role: MessageRole::User,
                content: "Follow up".into(),
            },
        ];
        let result = provider
            .complete("gpt-4o", &messages, 0.2, 4096, None)
            .await;
        assert!(result.is_ok());
    }

    #[test]
    fn gemini_flash_lite_cost() {
        let cost = estimate_openai_cost("gemini", "gemini-2.5-flash-lite", 1_000_000, 1_000_000);
        assert!((cost - 0.375).abs() < 0.01);
    }

    #[test]
    fn gemini_flash_cost() {
        let cost = estimate_openai_cost("gemini", "gemini-2.5-flash", 1_000_000, 1_000_000);
        assert!((cost - 0.75).abs() < 0.01);
    }

    #[test]
    fn gemini_pro_cost() {
        let cost = estimate_openai_cost("gemini", "gemini-pro", 1_000_000, 1_000_000);
        assert!((cost - 6.25).abs() < 0.01);
    }

    #[test]
    fn gemini_unknown_defaults_to_flash() {
        let cost = estimate_openai_cost("gemini", "gemini-whatever", 1_000_000, 1_000_000);
        assert!((cost - 0.75).abs() < 0.01);
    }

    #[test]
    fn openai_gpt4o_mini_cost() {
        let cost = estimate_openai_cost("openai", "gpt-4o-mini", 1_000_000, 1_000_000);
        assert!((cost - 0.75).abs() < 0.01);
    }

    #[test]
    fn openai_gpt4o_cost() {
        let cost = estimate_openai_cost("openai", "gpt-4o", 1_000_000, 1_000_000);
        assert!((cost - 12.5).abs() < 0.01);
    }

    #[test]
    fn openai_o1_cost() {
        let cost = estimate_openai_cost("openai", "o1-preview", 1_000_000, 1_000_000);
        assert!((cost - 50.0).abs() < 0.01);
    }

    #[test]
    fn openai_o3_cost() {
        let cost = estimate_openai_cost("openai", "o3-mini", 1_000_000, 1_000_000);
        assert!((cost - 50.0).abs() < 0.01);
    }

    #[test]
    fn openai_unknown_defaults() {
        let cost = estimate_openai_cost("openai", "unknown", 1_000_000, 1_000_000);
        assert!((cost - 12.5).abs() < 0.01);
    }

    #[test]
    fn zero_tokens_cost() {
        let cost = estimate_openai_cost("openai", "gpt-4o", 0, 0);
        assert!((cost - 0.0).abs() < f64::EPSILON);
    }
}
