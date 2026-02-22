use std::collections::HashMap;
use std::time::Instant;

use super::{Provider, ProviderError, ProviderFuture};
use crate::response::RouterResponse;
use glitchlab_kernel::agent::{ContentBlock, Message, MessageContent, MessageRole};
use glitchlab_kernel::tool::ToolDefinition;
use reqwest::Client;
use serde::{Deserialize, Serialize};

const GEMINI_API_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";

pub struct GeminiProvider {
    client: Client,
    api_key: String,
    base_url: String,
}

impl GeminiProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url: GEMINI_API_URL.into(),
        }
    }

    pub fn from_env() -> Result<Self, ProviderError> {
        let key = std::env::var("GOOGLE_API_KEY")
            .or_else(|_| std::env::var("GEMINI_API_KEY"))
            .map_err(|_| ProviderError::MissingApiKey("GOOGLE_API_KEY or GEMINI_API_KEY".into()))?;
        match std::env::var("GEMINI_BASE_URL") {
            Ok(url) => Ok(Self::with_base_url(key, url)),
            Err(_) => Ok(Self::new(key)),
        }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            base_url,
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Build the full URL: `{base_url}/{model}:generateContent?key={api_key}`
    fn url(&self, model: &str) -> String {
        format!(
            "{}/{}:generateContent?key={}",
            self.base_url, model, self.api_key
        )
    }

    /// POST a JSON body to the Gemini API, handle status codes,
    /// return `(response_text, latency_ms)`.
    async fn send_request(
        &self,
        model: &str,
        body: &impl Serialize,
    ) -> Result<(String, u64), ProviderError> {
        let start = Instant::now();

        let resp = self
            .client
            .post(self.url(model))
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

        Ok((resp_text, latency_ms))
    }

    /// Convert kernel `Message`s into Gemini format.
    ///
    /// Returns `(system_instruction, contents)`:
    /// - System messages are extracted into a single `system_instruction`.
    /// - Remaining messages are converted to Gemini `contents` entries.
    /// - Consecutive messages with the same role are merged (Gemini rejects these).
    /// - Tool results are mapped from `tool_call_id` back to function `name`.
    fn build_messages(
        messages: &[Message],
    ) -> (Option<GeminiSystemInstruction>, Vec<GeminiContent>) {
        // Extract system prompt.
        let system_text: String = messages
            .iter()
            .filter(|m| m.role == MessageRole::System)
            .map(|m| m.content.text())
            .collect::<Vec<_>>()
            .join("\n\n");

        let system_instruction = if system_text.is_empty() {
            None
        } else {
            Some(GeminiSystemInstruction {
                parts: vec![GeminiPart::Text { text: system_text }],
            })
        };

        // Build a mapping from tool_call_id → function_name by scanning all
        // ToolUse blocks in the conversation. This is needed because Gemini
        // functionResponse uses `name` (not an ID).
        let mut id_to_name: HashMap<String, String> = HashMap::new();
        for m in messages {
            if let MessageContent::Blocks(blocks) = &m.content {
                for block in blocks {
                    if let ContentBlock::ToolUse(tc) = block {
                        id_to_name.insert(tc.id.clone(), tc.name.clone());
                    }
                }
            }
        }

        // Convert non-system messages to Gemini parts, then merge consecutive same-role.
        let mut raw: Vec<GeminiContent> = Vec::new();
        for m in messages {
            if m.role == MessageRole::System {
                continue;
            }

            let role = match m.role {
                MessageRole::User | MessageRole::Tool => "user",
                MessageRole::Assistant => "model",
                MessageRole::System => unreachable!(),
            };

            let parts = match &m.content {
                MessageContent::Text(s) => vec![GeminiPart::Text { text: s.clone() }],
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => GeminiPart::Text { text: text.clone() },
                        ContentBlock::ToolUse(tc) => GeminiPart::FunctionCall {
                            function_call: GeminiFunctionCall {
                                name: tc.name.clone(),
                                args: tc.input.clone(),
                            },
                        },
                        ContentBlock::ToolResult(tr) => {
                            let name = id_to_name
                                .get(&tr.tool_call_id)
                                .cloned()
                                .unwrap_or_else(|| tr.tool_call_id.clone());
                            GeminiPart::FunctionResponse {
                                function_response: GeminiFunctionResponse {
                                    name,
                                    response: serde_json::json!({
                                        "content": tr.content,
                                        "is_error": tr.is_error,
                                    }),
                                },
                            }
                        }
                    })
                    .collect(),
            };

            raw.push(GeminiContent {
                role: role.into(),
                parts,
            });
        }

        // Merge consecutive entries with the same role.
        let contents = merge_consecutive(raw);

        (system_instruction, contents)
    }

    /// Parse a Gemini generateContent response.
    fn parse_response(
        resp_text: &str,
        model: &str,
        latency_ms: u64,
    ) -> Result<RouterResponse, ProviderError> {
        let parsed: GeminiResponse = serde_json::from_str(resp_text)
            .map_err(|e| ProviderError::Parse(format!("{e}: {resp_text}")))?;

        let candidate = parsed
            .candidates
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Parse("no candidates in response".into()))?;

        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut call_idx = 0u32;

        for part in candidate.content.parts {
            match part {
                GeminiResponsePart::Text { text } => {
                    text_parts.push(text);
                }
                GeminiResponsePart::FunctionCall { function_call } => {
                    let id = format!("gemini_call_{call_idx}");
                    call_idx += 1;
                    tool_calls.push(glitchlab_kernel::tool::ToolCall {
                        id,
                        name: function_call.name,
                        input: function_call.args,
                    });
                }
                GeminiResponsePart::Unknown => {}
            }
        }

        let content = text_parts.join("");

        let (prompt_tokens, completion_tokens, total_tokens) = match parsed.usage_metadata {
            Some(u) => (
                u.prompt_token_count,
                u.candidates_token_count,
                u.total_token_count,
            ),
            None => (0, 0, 0),
        };

        let cost = estimate_gemini_cost(model, prompt_tokens, completion_tokens);

        let stop_reason = candidate.finish_reason.map(|r| match r.as_str() {
            "STOP" => "end_turn".into(),
            "MAX_TOKENS" => "max_tokens".into(),
            other => other.to_string(),
        });

        Ok(RouterResponse {
            content,
            model: format!("gemini/{model}"),
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cost,
            latency_ms,
            tool_calls,
            stop_reason,
        })
    }
}

impl Provider for GeminiProvider {
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
            let (system_instruction, contents) = Self::build_messages(&messages);

            let body = GeminiRequest {
                system_instruction: system_instruction.as_ref(),
                contents: &contents,
                tools: None,
                generation_config: GeminiGenerationConfig {
                    temperature,
                    max_output_tokens: max_tokens,
                },
            };

            let (resp_text, latency_ms) = self.send_request(&model, &body).await?;
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
            let (system_instruction, contents) = Self::build_messages(&messages);

            let function_declarations: Vec<GeminiFunctionDeclaration<'_>> = tools
                .iter()
                .map(|t| GeminiFunctionDeclaration {
                    name: &t.name,
                    description: &t.description,
                    parameters: &t.input_schema,
                })
                .collect();

            let tool_defs = vec![GeminiToolDef {
                function_declarations: &function_declarations,
            }];

            let body = GeminiRequest {
                system_instruction: system_instruction.as_ref(),
                contents: &contents,
                tools: Some(&tool_defs),
                generation_config: GeminiGenerationConfig {
                    temperature,
                    max_output_tokens: max_tokens,
                },
            };

            let (resp_text, latency_ms) = self.send_request(&model, &body).await?;
            Self::parse_response(&resp_text, &model, latency_ms)
        })
    }
}

// ---------------------------------------------------------------------------
// Consecutive-role merging
// ---------------------------------------------------------------------------

/// Merge consecutive `GeminiContent` entries that share the same role.
/// Gemini rejects two consecutive messages with the same role.
fn merge_consecutive(contents: Vec<GeminiContent>) -> Vec<GeminiContent> {
    let mut merged: Vec<GeminiContent> = Vec::with_capacity(contents.len());
    for entry in contents {
        if let Some(last) = merged.last_mut()
            && last.role == entry.role
        {
            last.parts.extend(entry.parts);
            continue;
        }
        merged.push(entry);
    }
    merged
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct GeminiRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<&'a GeminiSystemInstruction>,
    contents: &'a [GeminiContent],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [GeminiToolDef<'a>]>,
    #[serde(rename = "generationConfig")]
    generation_config: GeminiGenerationConfig,
}

#[derive(Serialize)]
struct GeminiSystemInstruction {
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Clone)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize, Clone)]
#[serde(untagged)]
enum GeminiPart {
    Text {
        text: String,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCall,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GeminiFunctionResponse,
    },
}

#[derive(Serialize, Clone)]
struct GeminiFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Serialize, Clone)]
struct GeminiFunctionResponse {
    name: String,
    response: serde_json::Value,
}

#[derive(Serialize)]
struct GeminiGenerationConfig {
    temperature: f32,
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
}

#[derive(Serialize)]
struct GeminiToolDef<'a> {
    #[serde(rename = "functionDeclarations")]
    function_declarations: &'a [GeminiFunctionDeclaration<'a>],
}

#[derive(Serialize)]
struct GeminiFunctionDeclaration<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: GeminiCandidateContent,
    #[serde(rename = "finishReason")]
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct GeminiCandidateContent {
    parts: Vec<GeminiResponsePart>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GeminiResponsePart {
    Text {
        text: String,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiResponseFunctionCall,
    },
    Unknown,
}

#[derive(Deserialize)]
struct GeminiResponseFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Deserialize)]
struct GeminiUsageMetadata {
    #[serde(rename = "promptTokenCount")]
    #[serde(default)]
    prompt_token_count: u64,
    #[serde(rename = "candidatesTokenCount")]
    #[serde(default)]
    candidates_token_count: u64,
    #[serde(rename = "totalTokenCount")]
    #[serde(default)]
    total_token_count: u64,
}

// ---------------------------------------------------------------------------
// Cost estimation
// ---------------------------------------------------------------------------

fn estimate_gemini_cost(model: &str, prompt_tokens: u64, completion_tokens: u64) -> f64 {
    let (input_per_m, output_per_m) = if model.contains("flash-lite") {
        (0.075, 0.30)
    } else if model.contains("flash") {
        (0.15, 0.60)
    } else if model.contains("pro") {
        (1.25, 5.00)
    } else {
        // Default to flash pricing for unknown models.
        (0.15, 0.60)
    };

    let input_cost = (prompt_tokens as f64 / 1_000_000.0) * input_per_m;
    let output_cost = (completion_tokens as f64 / 1_000_000.0) * output_per_m;
    input_cost + output_cost
}

#[cfg(test)]
mod tests {
    use super::*;
    use glitchlab_kernel::agent::{Message, MessageContent, MessageRole};
    use glitchlab_kernel::tool::{ToolCall, ToolCallResult};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn mock_server(status: u16, body: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Return just the base origin — the provider builds the full path.
        let url = format!("http://{addr}");
        let status_line = match status {
            200 => "200 OK",
            429 => "429 Too Many Requests",
            529 => "529 Overloaded",
            _ => "500 Internal Server Error",
        };
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16384];
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

    fn gemini_success_body() -> String {
        serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": "{\"result\": \"ok\"}"}],
                    "role": "model"
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 50,
                "totalTokenCount": 150
            }
        })
        .to_string()
    }

    // -----------------------------------------------------------------------
    // build_messages unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_messages_system_extracted() {
        let messages = vec![
            Message {
                role: MessageRole::System,
                content: MessageContent::Text("You are a helper.".into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("Hi".into()),
            },
        ];
        let (sys, contents) = GeminiProvider::build_messages(&messages);
        assert!(sys.is_some());
        let sys = sys.unwrap();
        assert_eq!(sys.parts.len(), 1);
        // System should not appear in contents.
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "user");
    }

    #[test]
    fn build_messages_empty_system() {
        let messages = vec![Message {
            role: MessageRole::User,
            content: MessageContent::Text("Hi".into()),
        }];
        let (sys, contents) = GeminiProvider::build_messages(&messages);
        assert!(sys.is_none());
        assert_eq!(contents.len(), 1);
    }

    #[test]
    fn build_messages_role_mapping() {
        let messages = vec![
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("Hi".into()),
            },
            Message {
                role: MessageRole::Assistant,
                content: MessageContent::Text("Hello".into()),
            },
        ];
        let (_, contents) = GeminiProvider::build_messages(&messages);
        assert_eq!(contents[0].role, "user");
        assert_eq!(contents[1].role, "model");
    }

    #[test]
    fn build_messages_tool_use_becomes_function_call() {
        let messages = vec![Message {
            role: MessageRole::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse(ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "src/lib.rs"}),
            })]),
        }];
        let (_, contents) = GeminiProvider::build_messages(&messages);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].role, "model");
        // Verify it serializes as functionCall.
        let json = serde_json::to_value(&contents[0].parts[0]).unwrap();
        assert!(json.get("functionCall").is_some());
        assert_eq!(json["functionCall"]["name"], "read_file");
    }

    #[test]
    fn build_messages_tool_result_maps_id_to_name() {
        let messages = vec![
            Message {
                role: MessageRole::Assistant,
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse(ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                })]),
            },
            Message {
                role: MessageRole::Tool,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult(ToolCallResult {
                    tool_call_id: "call_1".into(),
                    content: "file contents here".into(),
                    is_error: false,
                })]),
            },
        ];
        let (_, contents) = GeminiProvider::build_messages(&messages);
        // Tool result should be mapped via id_to_name.
        let json = serde_json::to_value(&contents[1].parts[0]).unwrap();
        assert_eq!(json["functionResponse"]["name"], "read_file");
    }

    #[test]
    fn build_messages_consecutive_user_merged() {
        let messages = vec![
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("First".into()),
            },
            Message {
                role: MessageRole::User,
                content: MessageContent::Text("Second".into()),
            },
        ];
        let (_, contents) = GeminiProvider::build_messages(&messages);
        // Should be merged into one entry.
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].parts.len(), 2);
    }

    #[test]
    fn build_messages_multiple_tool_results_merged() {
        // User message followed by tool result (both map to "user" role).
        let messages = vec![
            Message {
                role: MessageRole::Assistant,
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolUse(ToolCall {
                        id: "c1".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path": "a.rs"}),
                    }),
                    ContentBlock::ToolUse(ToolCall {
                        id: "c2".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path": "b.rs"}),
                    }),
                ]),
            },
            Message {
                role: MessageRole::Tool,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult(ToolCallResult {
                    tool_call_id: "c1".into(),
                    content: "contents a".into(),
                    is_error: false,
                })]),
            },
            Message {
                role: MessageRole::Tool,
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult(ToolCallResult {
                    tool_call_id: "c2".into(),
                    content: "contents b".into(),
                    is_error: false,
                })]),
            },
        ];
        let (_, contents) = GeminiProvider::build_messages(&messages);
        // model message + merged user (tool results) = 2 entries.
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0].role, "model");
        assert_eq!(contents[1].role, "user");
        // Both tool results should be in the merged user entry.
        assert_eq!(contents[1].parts.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Integration tests (mock_server)
    // -----------------------------------------------------------------------

    #[test]
    fn provider_construction() {
        let _p = GeminiProvider::new("key".into());
    }

    #[test]
    fn provider_with_base_url() {
        let p = GeminiProvider::with_base_url("key".into(), "http://localhost:1234".into());
        assert_eq!(p.base_url, "http://localhost:1234");
    }

    #[tokio::test]
    async fn complete_success() {
        let url = mock_server(200, gemini_success_body()).await;
        let provider = GeminiProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete("gemini-2.5-flash", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.content.contains("result"));
        assert_eq!(response.prompt_tokens, 100);
        assert_eq!(response.completion_tokens, 50);
        assert_eq!(response.total_tokens, 150);
        assert!(response.model.contains("gemini"));
    }

    #[tokio::test]
    async fn complete_rate_limited() {
        let url = mock_server(429, "{}".into()).await;
        let provider = GeminiProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete("gemini-2.5-flash", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(matches!(result, Err(ProviderError::RateLimited { .. })));
    }

    #[tokio::test]
    async fn complete_api_error() {
        let url = mock_server(500, r#"{"error": "internal"}"#.into()).await;
        let provider = GeminiProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete("gemini-2.5-flash", &test_messages(), 0.2, 4096, None)
            .await;
        assert!(matches!(result, Err(ProviderError::Api { .. })));
    }

    #[tokio::test]
    async fn complete_stop_reason_populated() {
        let url = mock_server(200, gemini_success_body()).await;
        let provider = GeminiProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete("gemini-2.5-flash", &test_messages(), 0.2, 4096, None)
            .await
            .unwrap();
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
    }

    #[tokio::test]
    async fn complete_with_tools_text_only() {
        let body = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{"text": "No tools needed."}],
                    "role": "model"
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 50,
                "totalTokenCount": 150
            }
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = GeminiProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete_with_tools(
                "gemini-2.5-flash",
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
        assert_eq!(result.stop_reason.as_deref(), Some("end_turn"));
    }

    #[tokio::test]
    async fn complete_with_tools_single_function_call() {
        let body = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "name": "read_file",
                            "args": {"path": "src/lib.rs"}
                        }
                    }],
                    "role": "model"
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 50,
                "totalTokenCount": 150
            }
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = GeminiProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete_with_tools(
                "gemini-2.5-flash",
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
        assert_eq!(result.tool_calls[0].id, "gemini_call_0");
        assert_eq!(result.tool_calls[0].name, "read_file");
        assert_eq!(result.tool_calls[0].input["path"], "src/lib.rs");
    }

    #[tokio::test]
    async fn complete_with_tools_multiple_function_calls() {
        let body = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "Checking files."},
                        {
                            "functionCall": {
                                "name": "read_file",
                                "args": {"path": "a.rs"}
                            }
                        },
                        {
                            "functionCall": {
                                "name": "read_file",
                                "args": {"path": "b.rs"}
                            }
                        }
                    ],
                    "role": "model"
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 80,
                "totalTokenCount": 180
            }
        });
        let url = mock_server(200, body.to_string()).await;
        let provider = GeminiProvider::with_base_url("test-key".into(), url);
        let result = provider
            .complete_with_tools(
                "gemini-2.5-flash",
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
        assert_eq!(result.tool_calls[0].id, "gemini_call_0");
        assert_eq!(result.tool_calls[0].input["path"], "a.rs");
        assert_eq!(result.tool_calls[1].id, "gemini_call_1");
        assert_eq!(result.tool_calls[1].input["path"], "b.rs");
    }

    // -----------------------------------------------------------------------
    // Cost tests
    // -----------------------------------------------------------------------

    #[test]
    fn cost_flash_lite() {
        let cost = estimate_gemini_cost("gemini-2.5-flash-lite", 1_000_000, 1_000_000);
        assert!((cost - 0.375).abs() < 0.01);
    }

    #[test]
    fn cost_flash() {
        let cost = estimate_gemini_cost("gemini-2.5-flash", 1_000_000, 1_000_000);
        assert!((cost - 0.75).abs() < 0.01);
    }

    #[test]
    fn cost_pro() {
        let cost = estimate_gemini_cost("gemini-pro", 1_000_000, 1_000_000);
        assert!((cost - 6.25).abs() < 0.01);
    }

    #[test]
    fn cost_unknown_defaults() {
        let cost = estimate_gemini_cost("gemini-whatever", 1_000_000, 1_000_000);
        assert!((cost - 0.75).abs() < 0.01);
    }

    #[test]
    fn cost_zero_tokens() {
        let cost = estimate_gemini_cost("gemini-2.5-flash", 0, 0);
        assert!((cost - 0.0).abs() < f64::EPSILON);
    }
}
