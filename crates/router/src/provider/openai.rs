use std::time::Instant;

use glitchlab_kernel::agent::{Message, MessageRole};
use glitchlab_kernel::tool::ToolDefinition;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{Provider, ProviderError, ProviderFuture};
use crate::response::RouterResponse;

const OPENAI_API_URL: &str = "https://api.openai.com/v1/chat/completions";

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

    pub fn custom(api_key: String, base_url: String, name: String) -> Self {
        Self::new(api_key, base_url, name)
    }

    // -----------------------------------------------------------------------
    // Private helpers — shared by complete() and complete_with_tools()
    // -----------------------------------------------------------------------

    /// POST a JSON body to the OpenAI-compatible API, handle status codes,
    /// return `(response_text, latency_ms)`.
    async fn send_request(&self, body: &impl Serialize) -> Result<(String, u64), ProviderError> {
        let start = Instant::now();

        let resp = self
            .client
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
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

    /// Convert kernel `Message`s into OpenAI format.
    fn build_messages(messages: &[Message]) -> Vec<OaiMessage> {
        messages
            .iter()
            .map(|m| OaiMessage {
                role: match m.role {
                    MessageRole::System => "system".into(),
                    MessageRole::User => "user".into(),
                    MessageRole::Assistant => "assistant".into(),
                    MessageRole::Tool => "tool".into(),
                },
                content: m.content.text(),
            })
            .collect()
    }

    /// Parse an OpenAI-compatible response body, extracting text, tool calls,
    /// usage, finish_reason, and cost.
    fn parse_response(
        &self,
        resp_text: &str,
        model: &str,
        latency_ms: u64,
    ) -> Result<RouterResponse, ProviderError> {
        let parsed: OaiResponse = serde_json::from_str(resp_text)
            .map_err(|e| ProviderError::Parse(format!("{e}: {resp_text}")))?;

        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Parse("no choices in response".into()))?;

        let content = choice.message.content.unwrap_or_default();

        let mut tool_calls = Vec::new();
        for oai_tc in choice.message.tool_calls {
            match serde_json::from_str::<serde_json::Value>(&oai_tc.function.arguments) {
                Ok(input) => {
                    tool_calls.push(glitchlab_kernel::tool::ToolCall {
                        id: oai_tc.id,
                        name: oai_tc.function.name,
                        input,
                    });
                }
                Err(e) => {
                    warn!(
                        tool_call_id = %oai_tc.id,
                        tool_name = %oai_tc.function.name,
                        error = %e,
                        "skipping tool call with malformed arguments"
                    );
                }
            }
        }

        let prompt_tokens = parsed.usage.prompt_tokens;
        let completion_tokens = parsed.usage.completion_tokens;
        let total_tokens = parsed.usage.total_tokens;
        let cost =
            estimate_openai_cost(&self.provider_name, model, prompt_tokens, completion_tokens);

        Ok(RouterResponse {
            content,
            model: format!("{}/{model}", self.provider_name),
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cost,
            latency_ms,
            tool_calls,
            stop_reason: choice.finish_reason,
        })
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
            let oai_messages = Self::build_messages(&messages);

            let body = OaiRequest {
                model: &model,
                messages: &oai_messages,
                temperature,
                max_tokens,
                response_format: response_format.as_ref(),
            };

            let (resp_text, latency_ms) = self.send_request(&body).await?;
            self.parse_response(&resp_text, &model, latency_ms)
        })
    }

    fn complete_with_tools(
        &self,
        model: &str,
        messages: &[Message],
        temperature: f32,
        max_tokens: u32,
        tools: &[ToolDefinition],
        response_format: Option<&serde_json::Value>,
    ) -> ProviderFuture<'_> {
        let model = model.to_string();
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        let response_format = response_format.cloned();

        Box::pin(async move {
            let oai_messages = Self::build_messages(&messages);

            let tool_defs: Vec<OaiToolDef<'_>> = tools
                .iter()
                .map(|t| OaiToolDef {
                    r#type: "function",
                    function: OaiFunctionDef {
                        name: &t.name,
                        description: &t.description,
                        parameters: &t.input_schema,
                    },
                })
                .collect();

            let body = OaiToolRequest {
                model: &model,
                messages: &oai_messages,
                temperature,
                max_tokens,
                response_format: response_format.as_ref(),
                tools: &tool_defs,
            };

            let (resp_text, latency_ms) = self.send_request(&body).await?;
            self.parse_response(&resp_text, &model, latency_ms)
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
struct OaiToolRequest<'a> {
    model: &'a str,
    messages: &'a [OaiMessage],
    temperature: f32,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<&'a serde_json::Value>,
    tools: &'a [OaiToolDef<'a>],
}

#[derive(Serialize)]
struct OaiToolDef<'a> {
    r#type: &'static str,
    function: OaiFunctionDef<'a>,
}

#[derive(Serialize)]
struct OaiFunctionDef<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
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
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct OaiChoiceMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OaiToolCall>,
}

#[derive(Deserialize)]
struct OaiToolCall {
    id: String,
    function: OaiToolCallFunction,
}

#[derive(Deserialize)]
struct OaiToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct OaiUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

fn estimate_openai_cost(
    _provider: &str,
    model: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> f64 {
    let (input_per_m, output_per_m) = if model.contains("gpt-4o-mini") {
        (0.15, 0.60)
    } else if model.contains("gpt-4o") {
        (2.50, 10.0)
    } else if model.contains("o1") || model.contains("o3") {
        (10.0, 40.0)
    } else {
        (2.50, 10.0)
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
    async fn complete_overloaded_529() {
        let url = mock_server(529, "{}".into()).await;
        let provider = OpenAiProvider::new("test-key".into(), url, "openai".into());
        let result = provider
            .complete("gpt-4o", &test_messages(), 0.2, 4096, None)
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
        let provider = OpenAiProvider::new("test-key".into(), url, "openai".into());
        let result = provider
            .complete("gpt-4o", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(matches!(result, Err(ProviderError::Api { .. })));
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
                content: MessageContent::Text("System".into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("Hi".into()),
            },
            Message {
                role: MessageRole::Assistant,
                content: MessageContent::Text("Hello".into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("Follow up".into()),
            },
        ];
        let result = provider
            .complete("gpt-4o", &messages, 0.2, 4096, None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn complete_finish_reason_populated() {
        let body = serde_json::json!({
            "choices": [{"message": {"content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = OpenAiProvider::new("test-key".into(), url, "openai".into());
        let result = provider
            .complete("gpt-4o", &test_messages(), 0.2, 4096, None)
            .await
            .unwrap();
        assert_eq!(result.stop_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn complete_with_tools_text_only() {
        let body = serde_json::json!({
            "choices": [{"message": {"content": "No tools needed."}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150}
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = OpenAiProvider::new("test-key".into(), url, "openai".into());
        let result = provider
            .complete_with_tools(
                "gpt-4o",
                &test_messages(),
                0.2,
                4096,
                &test_tool_defs(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.content, "No tools needed.");
        assert!(result.tool_calls.is_empty());
        assert_eq!(result.stop_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn complete_with_tools_single_tool_call() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"src/lib.rs\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 100, "completion_tokens": 50, "total_tokens": 150}
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = OpenAiProvider::new("test-key".into(), url, "openai".into());
        let result = provider
            .complete_with_tools(
                "gpt-4o",
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
        assert_eq!(result.tool_calls[0].id, "call_abc");
        assert_eq!(result.tool_calls[0].name, "read_file");
        assert_eq!(result.tool_calls[0].input["path"], "src/lib.rs");
        assert_eq!(result.stop_reason.as_deref(), Some("tool_calls"));
    }

    #[tokio::test]
    async fn complete_with_tools_multiple_tool_calls() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Checking files.",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\":\"a.rs\"}"
                            }
                        },
                        {
                            "id": "call_2",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\":\"b.rs\"}"
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 100, "completion_tokens": 80, "total_tokens": 180}
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = OpenAiProvider::new("test-key".into(), url, "openai".into());
        let result = provider
            .complete_with_tools(
                "gpt-4o",
                &test_messages(),
                0.2,
                4096,
                &test_tool_defs(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.content, "Checking files.");
        assert_eq!(result.tool_calls.len(), 2);
        assert_eq!(result.tool_calls[0].id, "call_1");
        assert_eq!(result.tool_calls[0].input["path"], "a.rs");
        assert_eq!(result.tool_calls[1].id, "call_2");
        assert_eq!(result.tool_calls[1].input["path"], "b.rs");
    }

    #[tokio::test]
    async fn complete_with_tools_malformed_arguments() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_bad",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "not valid json {{{"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = OpenAiProvider::new("test-key".into(), url, "openai".into());
        let result = provider
            .complete_with_tools(
                "gpt-4o",
                &test_messages(),
                0.2,
                4096,
                &test_tool_defs(),
                None,
            )
            .await
            .unwrap();
        // Malformed tool call is gracefully skipped.
        assert!(result.tool_calls.is_empty());
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
