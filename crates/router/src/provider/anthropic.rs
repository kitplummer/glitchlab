use std::time::Instant;

use glitchlab_kernel::agent::{ContentBlock, Message, MessageContent, MessageRole};
use glitchlab_kernel::tool::ToolDefinition;
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
        match std::env::var("ANTHROPIC_BASE_URL") {
            Ok(url) => Ok(Self::with_base_url(key, url)),
            Err(_) => Ok(Self::new(key)),
        }
    }

    /// Custom base URL (for testing or proxying).
    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url,
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers — shared by complete() and complete_with_tools()
    // -----------------------------------------------------------------------

    /// POST a JSON body to the Anthropic API, handle status codes,
    /// return `(response_text, latency_ms)`.
    async fn send_request(&self, body: &impl Serialize) -> Result<(String, u64), ProviderError> {
        let start = Instant::now();

        let resp = self
            .client
            .post(&self.base_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await?;

        let latency_ms = start.elapsed().as_millis() as u64;
        let status = resp.status().as_u16();

        if status == 429 || status == 529 {
            return Err(ProviderError::RateLimited {
                retry_after_ms: resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|s| s * 1000)
                    .or(if status == 529 { Some(5000) } else { None }),
            });
        }

        let resp_text = resp.text().await?;

        if status >= 400 {
            return Err(ProviderError::Api {
                status,
                body: resp_text,
            });
        }

        Ok((resp_text, latency_ms))
    }

    /// Convert kernel `Message`s into Anthropic format, separating the
    /// system prompt from conversation messages.
    ///
    /// Text-only messages use a plain string `content`.  Messages with
    /// tool-use or tool-result blocks are serialized as the structured
    /// `content: [...]` array that the Anthropic API expects.
    fn build_messages(messages: &[Message]) -> (String, Vec<AnthropicMessage>) {
        let system: String = messages
            .iter()
            .filter(|m| m.role == MessageRole::System)
            .map(|m| m.content.text())
            .collect::<Vec<_>>()
            .join("\n\n");

        let conversation: Vec<_> = messages
            .iter()
            .filter(|m| m.role != MessageRole::System)
            .map(|m| {
                let role = match m.role {
                    MessageRole::User | MessageRole::Tool => "user".into(),
                    MessageRole::Assistant => "assistant".into(),
                    MessageRole::System => unreachable!(),
                };

                let content = match &m.content {
                    MessageContent::Text(s) => serde_json::Value::String(s.clone()),
                    MessageContent::Blocks(blocks) => {
                        let arr: Vec<serde_json::Value> = blocks
                            .iter()
                            .map(|b| match b {
                                ContentBlock::Text { text } => {
                                    serde_json::json!({"type": "text", "text": text})
                                }
                                ContentBlock::ToolUse(tc) => {
                                    serde_json::json!({
                                        "type": "tool_use",
                                        "id": tc.id,
                                        "name": tc.name,
                                        "input": tc.input,
                                    })
                                }
                                ContentBlock::ToolResult(tr) => {
                                    serde_json::json!({
                                        "type": "tool_result",
                                        "tool_use_id": tr.tool_call_id,
                                        "content": tr.content,
                                        "is_error": tr.is_error,
                                    })
                                }
                            })
                            .collect();
                        serde_json::Value::Array(arr)
                    }
                };

                AnthropicMessage { role, content }
            })
            .collect();

        (system, conversation)
    }

    /// Parse an Anthropic response body, extracting text, tool calls,
    /// usage, stop_reason, and cost.
    fn parse_response(
        resp_text: &str,
        model: &str,
        latency_ms: u64,
    ) -> Result<RouterResponse, ProviderError> {
        let parsed: AnthropicResponse = serde_json::from_str(resp_text)
            .map_err(|e| ProviderError::Parse(format!("{e}: {resp_text}")))?;

        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();

        for block in parsed.content {
            match block.r#type.as_str() {
                "text" => {
                    if let Some(text) = block.text {
                        text_parts.push(text);
                    }
                }
                "tool_use" => {
                    if let (Some(id), Some(name), Some(input)) = (block.id, block.name, block.input)
                    {
                        tool_calls.push(glitchlab_kernel::tool::ToolCall { id, name, input });
                    }
                }
                _ => {}
            }
        }

        let content = text_parts.join("");
        let prompt_tokens = parsed.usage.input_tokens;
        let completion_tokens = parsed.usage.output_tokens;
        let total_tokens = prompt_tokens + completion_tokens;
        let cost = estimate_anthropic_cost(model, prompt_tokens, completion_tokens);

        Ok(RouterResponse {
            content,
            model: format!("anthropic/{model}"),
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cost,
            latency_ms,
            tool_calls,
            stop_reason: parsed.stop_reason,
        })
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
            let (system, conversation) = Self::build_messages(&messages);

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

            let (resp_text, latency_ms) = self.send_request(&body).await?;
            Self::parse_response(&resp_text, &model, latency_ms)
        })
    }

    fn complete_with_tools(
        &self,
        model: &str,
        messages: &[Message],
        temperature: f32,
        max_tokens: u32,
        tools: &[ToolDefinition],
        _response_format: Option<&serde_json::Value>,
    ) -> ProviderFuture<'_> {
        let model = model.to_string();
        let messages = messages.to_vec();
        let tools = tools.to_vec();

        Box::pin(async move {
            let (system, conversation) = Self::build_messages(&messages);

            let tool_defs: Vec<AnthropicToolDef<'_>> = tools
                .iter()
                .map(|t| AnthropicToolDef {
                    name: &t.name,
                    description: &t.description,
                    input_schema: &t.input_schema,
                })
                .collect();

            let body = AnthropicToolRequest {
                model: &model,
                max_tokens,
                temperature,
                system: if system.is_empty() {
                    None
                } else {
                    Some(&system)
                },
                messages: &conversation,
                tools: &tool_defs,
            };

            let (resp_text, latency_ms) = self.send_request(&body).await?;
            Self::parse_response(&resp_text, &model, latency_ms)
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
struct AnthropicToolRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: &'a [AnthropicMessage],
    tools: &'a [AnthropicToolDef<'a>],
}

#[derive(Serialize)]
struct AnthropicToolDef<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: serde_json::Value,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<ResponseBlock>,
    usage: Usage,
    #[serde(default)]
    stop_reason: Option<String>,
}

/// A content block in an Anthropic API *response* (deserialized from JSON).
/// Separate from `glitchlab_kernel::agent::ContentBlock` which is the kernel type.
#[derive(Deserialize)]
struct ResponseBlock {
    r#type: String,
    text: Option<String>,
    id: Option<String>,
    name: Option<String>,
    input: Option<serde_json::Value>,
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
    use glitchlab_kernel::agent::{Message, MessageContent, MessageRole};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn mock_server(status: u16, body: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let status_line = match status {
            200 => "200 OK",
            429 => "429 Too Many Requests",
            529 => "529 Overloaded",
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
                content: MessageContent::Text("System prompt".into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("Hello".into()),
            },
        ]
    }

    fn test_tool_defs() -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "read_file".into(),
            description: "Read a file from disk".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        }]
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

    #[test]
    fn from_env_uses_base_url_when_set() {
        // Temporarily set env vars for this test.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "test-key-env");
            std::env::set_var("ANTHROPIC_BASE_URL", "http://localhost:9999");
        }
        let p = AnthropicProvider::from_env().unwrap();
        assert_eq!(p.base_url, "http://localhost:9999");
        assert_eq!(p.api_key, "test-key-env");
        unsafe {
            std::env::remove_var("ANTHROPIC_BASE_URL");
        }
    }

    #[tokio::test]
    async fn complete_success() {
        let body = serde_json::json!({
            "content": [{"type": "text", "text": "{\"result\": \"ok\"}"}],
            "usage": {"input_tokens": 100, "output_tokens": 50}
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = AnthropicProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete(
                "claude-sonnet-4-20250514",
                &test_messages(),
                0.2,
                4096,
                None,
            )
            .await;
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
        let result = provider
            .complete(
                "claude-sonnet-4-20250514",
                &test_messages(),
                0.2,
                4096,
                None,
            )
            .await;
        assert!(matches!(result, Err(ProviderError::RateLimited { .. })));
    }

    #[tokio::test]
    async fn complete_overloaded_529() {
        let url = mock_server(529, "{}".into()).await;
        let provider = AnthropicProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete(
                "claude-sonnet-4-20250514",
                &test_messages(),
                0.2,
                4096,
                None,
            )
            .await;
        match result {
            Err(ProviderError::RateLimited { retry_after_ms }) => {
                assert_eq!(retry_after_ms, Some(5000));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn complete_api_error() {
        let url = mock_server(500, r#"{"error": "internal"}"#.into()).await;
        let provider = AnthropicProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete(
                "claude-sonnet-4-20250514",
                &test_messages(),
                0.2,
                4096,
                None,
            )
            .await;
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
        let messages = vec![Message {
            role: MessageRole::User,
            content: MessageContent::Text("Hi".into()),
        }];
        let result = provider
            .complete("claude-haiku-3-20240307", &messages, 0.5, 1024, None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn complete_stop_reason_populated() {
        let body = serde_json::json!({
            "content": [{"type": "text", "text": "done"}],
            "usage": {"input_tokens": 10, "output_tokens": 5},
            "stop_reason": "end_turn"
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = AnthropicProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete(
                "claude-sonnet-4-20250514",
                &test_messages(),
                0.2,
                4096,
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
    }

    #[tokio::test]
    async fn complete_with_tools_text_only() {
        let body = serde_json::json!({
            "content": [{"type": "text", "text": "I don't need any tools."}],
            "usage": {"input_tokens": 100, "output_tokens": 50},
            "stop_reason": "end_turn"
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = AnthropicProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete_with_tools(
                "claude-sonnet-4-20250514",
                &test_messages(),
                0.2,
                4096,
                &test_tool_defs(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.content, "I don't need any tools.");
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
    }

    #[tokio::test]
    async fn complete_with_tools_single_tool_call() {
        let body = serde_json::json!({
            "content": [
                {"type": "tool_use", "id": "toolu_123", "name": "read_file", "input": {"path": "src/lib.rs"}}
            ],
            "usage": {"input_tokens": 100, "output_tokens": 50},
            "stop_reason": "tool_use"
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = AnthropicProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete_with_tools(
                "claude-sonnet-4-20250514",
                &test_messages(),
                0.2,
                4096,
                &test_tool_defs(),
                None,
            )
            .await
            .unwrap();
        assert!(result.content.is_empty());
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "toolu_123");
        assert_eq!(result.tool_calls[0].name, "read_file");
        assert_eq!(result.tool_calls[0].input["path"], "src/lib.rs");
        assert_eq!(result.stop_reason.as_deref(), Some("tool_use"));
    }

    #[tokio::test]
    async fn complete_with_tools_multiple_tool_calls() {
        let body = serde_json::json!({
            "content": [
                {"type": "text", "text": "Let me check those files."},
                {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a.rs"}},
                {"type": "tool_use", "id": "toolu_2", "name": "read_file", "input": {"path": "b.rs"}}
            ],
            "usage": {"input_tokens": 100, "output_tokens": 80},
            "stop_reason": "tool_use"
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = AnthropicProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete_with_tools(
                "claude-sonnet-4-20250514",
                &test_messages(),
                0.2,
                4096,
                &test_tool_defs(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.content, "Let me check those files.");
        assert_eq!(result.tool_calls.len(), 2);
        assert_eq!(result.tool_calls[0].id, "toolu_1");
        assert_eq!(result.tool_calls[0].input["path"], "a.rs");
        assert_eq!(result.tool_calls[1].id, "toolu_2");
        assert_eq!(result.tool_calls[1].input["path"], "b.rs");
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
