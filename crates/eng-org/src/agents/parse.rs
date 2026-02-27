use glitchlab_kernel::agent::{AgentMetadata, AgentOutput};
use tracing::warn;

/// Parse an LLM response as JSON, with graceful fallback.
///
/// Attempts:
/// 1. Direct JSON parse.
/// 2. Strip markdown code fences and retry.
/// 3. Extract first `{...}` block (handles nested braces).
/// 4. Sanitize extracted JSON (trailing commas, newlines in strings) and retry.
/// 5. Attempt to close truncated JSON (unbalanced braces) and retry.
/// 6. Return a fallback with `parse_error: true`.
pub fn parse_json_response(
    raw: &str,
    metadata: AgentMetadata,
    fallback: serde_json::Value,
) -> AgentOutput {
    // Attempt 1: direct parse.
    if let Ok(data) = serde_json::from_str::<serde_json::Value>(raw) {
        return AgentOutput {
            data,
            metadata,
            parse_error: false,
        };
    }

    // Attempt 2: strip markdown fences.
    let stripped = strip_code_fences(raw);
    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&stripped) {
        return AgentOutput {
            data,
            metadata,
            parse_error: false,
        };
    }

    // Attempt 3: extract first JSON object.
    if let Some(extracted) = extract_json_object(&stripped) {
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&extracted) {
            return AgentOutput {
                data,
                metadata,
                parse_error: false,
            };
        }

        // Attempt 4: sanitize common JSON issues (trailing commas, newlines).
        let sanitized = sanitize_json(&extracted);
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&sanitized) {
            return AgentOutput {
                data,
                metadata,
                parse_error: false,
            };
        }
    }

    // Attempt 5: close truncated JSON (unbalanced braces from max_tokens cutoff).
    if let Some(closed) = close_truncated_json(&stripped)
        && let Ok(data) = serde_json::from_str::<serde_json::Value>(&closed)
    {
        warn!(
            raw_len = raw.len(),
            "recovered truncated JSON response by closing braces"
        );
        return AgentOutput {
            data,
            metadata,
            parse_error: false,
        };
    }

    // Fallback.
    warn!(
        raw_len = raw.len(),
        raw_preview = &raw[..raw.len().min(500)],
        "failed to parse agent JSON response, using fallback"
    );
    AgentOutput {
        data: fallback,
        metadata,
        parse_error: true,
    }
}

/// Strip markdown code fences (```json ... ``` or ``` ... ```).
///
/// Handles both complete fences and truncated responses where the closing
/// fence is missing (e.g. from max_tokens cutoff).
fn strip_code_fences(s: &str) -> String {
    let trimmed = s.trim();

    // Try complete fences first (opening + closing).
    if let Some(rest) = trimmed.strip_prefix("```json")
        && let Some(inner) = rest.strip_suffix("```")
    {
        return inner.trim().to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("```")
        && let Some(inner) = rest.strip_suffix("```")
    {
        return inner.trim().to_string();
    }

    // Handle truncated response: opening fence present but no closing fence.
    // Strip just the opening ``` line so downstream parsers can work with the JSON.
    if let Some(rest) = trimmed.strip_prefix("```json") {
        return rest.trim().to_string();
    }
    if trimmed.starts_with("```") {
        // Strip the first line (``` or ```<lang>).
        if let Some(newline_pos) = trimmed.find('\n') {
            return trimmed[newline_pos + 1..].trim().to_string();
        }
    }

    trimmed.to_string()
}

/// Fix common JSON issues that LLMs produce:
/// - Trailing commas before `}` or `]`
/// - Literal newlines inside string values (replace with `\n`)
fn sanitize_json(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;

    while i < chars.len() {
        if escape {
            result.push(chars[i]);
            escape = false;
            i += 1;
            continue;
        }

        if chars[i] == '\\' && in_string {
            result.push(chars[i]);
            escape = true;
            i += 1;
            continue;
        }

        if chars[i] == '"' {
            in_string = !in_string;
            result.push(chars[i]);
            i += 1;
            continue;
        }

        if in_string {
            // Replace literal newlines inside strings with escape sequence.
            if chars[i] == '\n' {
                result.push_str("\\n");
                i += 1;
                continue;
            }
            if chars[i] == '\r' {
                i += 1;
                continue; // skip CR (the \n branch handles the newline)
            }
            result.push(chars[i]);
            i += 1;
            continue;
        }

        // Outside a string: check for trailing comma.
        if chars[i] == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Attempt to close truncated JSON by finding the first `{`, then appending
/// closing braces/brackets to balance the nesting.
///
/// This handles the common case where an LLM response was cut off by
/// `max_tokens`, leaving valid JSON except for missing closing characters.
/// Uses two strategies:
/// 1. Truncate to the last successfully closed `}` or `]`, then close remaining.
/// 2. If no brackets were closed, strip incomplete trailing tokens and close.
fn close_truncated_json(s: &str) -> Option<String> {
    let start = s.find('{')?;
    let bytes = s.as_bytes();
    let mut in_string = false;
    let mut escape = false;
    let mut stack: Vec<char> = Vec::new();
    // Track position after last `}` or `]` close where stack is still non-empty.
    let mut last_close: Option<(usize, Vec<char>)> = None;

    for (i, &b) in bytes[start..].iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        if b == b'\\' && in_string {
            escape = true;
            continue;
        }
        if b == b'"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match b {
            b'{' => stack.push('}'),
            b'[' => stack.push(']'),
            b'}' | b']' => {
                stack.pop();
                if stack.is_empty() {
                    return None; // already balanced
                }
                last_close = Some((start + i + 1, stack.clone()));
            }
            _ => {}
        }
    }

    if stack.is_empty() {
        return None;
    }

    // Strategy 1: truncate at last safe bracket close, close remaining.
    if let Some((pos, remaining_stack)) = last_close {
        let mut result = s[start..pos].to_string();
        let trimmed = result.trim_end_matches(|c: char| c == ',' || c.is_whitespace());
        result.truncate(trimmed.len());
        for closer in remaining_stack.iter().rev() {
            result.push(*closer);
        }
        return Some(result);
    }

    // Strategy 2: no brackets closed yet. Strip incomplete trailing tokens
    // and close all brackets. Only works if we're not mid-string.
    if in_string {
        return None;
    }

    let mut result = s[start..].to_string();
    // Strip partial keyword/number tokens (e.g. "fal" from truncated "false").
    let trimmed = result.trim_end_matches(|c: char| {
        c.is_alphanumeric() || c == '.' || c == '_' || c == '+' || c == '-'
    });
    // Strip trailing comma, colon, whitespace.
    let trimmed = trimmed.trim_end_matches(|c: char| c == ',' || c == ':' || c.is_whitespace());
    result.truncate(trimmed.len());

    for closer in stack.into_iter().rev() {
        result.push(closer);
    }
    Some(result)
}

/// Extract the first `{...}` block (handling nested braces).
fn extract_json_object(s: &str) -> Option<String> {
    let start = s.find('{')?;
    let bytes = s.as_bytes();
    let mut depth = 0;
    let mut in_string = false;
    let mut escape = false;

    for (i, &b) in bytes[start..].iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        if b == b'\\' && in_string {
            escape = true;
            continue;
        }
        if b == b'"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        if b == b'{' {
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(s[start..start + i + 1].to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clean_json() {
        let raw = r#"{"verdict": "pass", "issues": []}"#;
        let meta = test_meta();
        let output = parse_json_response(raw, meta, serde_json::json!({}));
        assert!(!output.parse_error);
        assert_eq!(output.data["verdict"], "pass");
    }

    #[test]
    fn parse_markdown_wrapped() {
        let raw = "```json\n{\"verdict\": \"pass\"}\n```";
        let meta = test_meta();
        let output = parse_json_response(raw, meta, serde_json::json!({}));
        assert!(!output.parse_error);
        assert_eq!(output.data["verdict"], "pass");
    }

    #[test]
    fn parse_with_preamble() {
        let raw = "Here is the result:\n{\"verdict\": \"pass\"}";
        let meta = test_meta();
        let output = parse_json_response(raw, meta, serde_json::json!({}));
        assert!(!output.parse_error);
    }

    #[test]
    fn parse_fallback_on_garbage() {
        let raw = "This is not JSON at all.";
        let meta = test_meta();
        let fallback = serde_json::json!({"error": true});
        let output = parse_json_response(raw, meta, fallback.clone());
        assert!(output.parse_error);
        assert_eq!(output.data, fallback);
    }

    #[test]
    fn parse_generic_code_fence() {
        let raw = "```\n{\"verdict\": \"pass\"}\n```";
        let meta = test_meta();
        let output = parse_json_response(raw, meta, serde_json::json!({}));
        assert!(!output.parse_error);
        assert_eq!(output.data["verdict"], "pass");
    }

    #[test]
    fn strip_fences_no_match() {
        let result = strip_code_fences("just plain text");
        assert_eq!(result, "just plain text");
    }

    #[test]
    fn extract_json_nested_braces() {
        let input = r#"prefix {"key": {"nested": true}} suffix"#;
        let extracted = extract_json_object(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(parsed["key"]["nested"], true);
    }

    #[test]
    fn extract_json_with_string_braces() {
        let input = r#"{"message": "use { and } in strings"}"#;
        let extracted = extract_json_object(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(parsed["message"], "use { and } in strings");
    }

    #[test]
    fn extract_json_none_when_no_json() {
        assert!(extract_json_object("no json here").is_none());
    }

    #[test]
    fn extract_json_unclosed_brace() {
        assert!(extract_json_object("{unclosed").is_none());
    }

    #[test]
    fn parse_json_with_trailing_comma() {
        let raw = r#"{"key": "value", "arr": [1, 2,], }"#;
        let meta = test_meta();
        let output = parse_json_response(raw, meta, serde_json::json!({}));
        assert!(!output.parse_error);
        assert_eq!(output.data["key"], "value");
    }

    #[test]
    fn parse_json_surrounded_by_text() {
        let raw = "Here is my analysis:\n\n```json\n{\"steps\": [{\"step_number\": 1}]}\n```\n\nLet me know if you need more.";
        let meta = test_meta();
        let output = parse_json_response(raw, meta, serde_json::json!({}));
        assert!(!output.parse_error);
        assert!(output.data["steps"].is_array());
    }

    #[test]
    fn parse_json_multiline_preamble() {
        let raw = "I'll create a plan for this task.\n\nThe plan:\n{\"steps\": [], \"risk_level\": \"low\"}";
        let meta = test_meta();
        let output = parse_json_response(raw, meta, serde_json::json!({}));
        assert!(!output.parse_error);
        assert_eq!(output.data["risk_level"], "low");
    }

    #[test]
    fn sanitize_json_trailing_comma_object() {
        let input = r#"{"a": 1, "b": 2, }"#;
        let result = sanitize_json(input);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["b"], 2);
    }

    #[test]
    fn sanitize_json_trailing_comma_array() {
        let input = r#"{"arr": [1, 2, 3, ]}"#;
        let result = sanitize_json(input);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["arr"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn sanitize_json_no_change_when_valid() {
        let input = r#"{"a": 1, "b": 2}"#;
        let result = sanitize_json(input);
        assert_eq!(result, input);
    }

    #[test]
    fn sanitize_json_preserves_commas_in_strings() {
        let input = r#"{"msg": "a, b, c,"}"#;
        let result = sanitize_json(input);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["msg"], "a, b, c,");
    }

    #[test]
    fn strip_fences_truncated_no_closing() {
        let input = "```json\n{\"key\": \"value\"";
        let result = strip_code_fences(input);
        assert_eq!(result, "{\"key\": \"value\"");
    }

    #[test]
    fn strip_fences_truncated_generic() {
        let input = "```rust\nfn main() {}";
        let result = strip_code_fences(input);
        assert_eq!(result, "fn main() {}");
    }

    #[test]
    fn parse_truncated_json_recovery() {
        // Simulate a response truncated by max_tokens: fenced, incomplete JSON.
        let raw = "```json\n{\n  \"steps\": [\n    {\n      \"step_number\": 1,\n      \"description\": \"Create build.rs\",\n      \"files\": [\"build.rs\"],\n      \"action\": \"create\"\n    }\n  ],\n  \"files_likely_affected\": [\"build.rs\"],\n  \"requires_core_change\": fal";
        let meta = test_meta();
        let output = parse_json_response(raw, meta, serde_json::json!({}));
        // Should recover by closing the truncated JSON.
        assert!(!output.parse_error, "should recover truncated JSON");
        assert!(output.data["steps"].is_array());
        assert_eq!(output.data["files_likely_affected"][0], "build.rs");
    }

    #[test]
    fn parse_truncated_json_mid_string() {
        // Truncated inside a string value.
        let raw = "```json\n{\"steps\": [{\"description\": \"Add a new fea";
        let meta = test_meta();
        let fallback = serde_json::json!({"fallback": true});
        let output = parse_json_response(raw, meta, fallback);
        // Truncated mid-string — too risky to recover. Falls back gracefully.
        assert!(output.parse_error);
    }

    #[test]
    fn close_truncated_json_basic() {
        let input = "{\"a\": 1, \"b\": [2, 3";
        let result = close_truncated_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["a"], 1);
    }

    #[test]
    fn close_truncated_json_returns_none_when_balanced() {
        let input = "{\"a\": 1}";
        assert!(close_truncated_json(input).is_none());
    }

    #[test]
    fn close_truncated_json_nested() {
        let input = "{\"steps\": [{\"n\": 1}], \"risk\": \"lo";
        let result = close_truncated_json(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["steps"][0]["n"], 1);
    }

    #[test]
    fn sanitize_json_literal_newline_in_string() {
        let input = "{\"desc\": \"line one\nline two\"}";
        let result = sanitize_json(input);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["desc"], "line one\nline two");
    }

    #[test]
    fn sanitize_json_preserves_escaped_newline() {
        let input = r#"{"desc": "line one\nline two"}"#;
        let result = sanitize_json(input);
        // Already-escaped \n should be preserved as-is.
        assert_eq!(result, input);
    }

    #[test]
    fn parse_malformed_json_fails_gracefully() {
        // This JSON is malformed because the string "world" is not quoted.
        let raw = r#"{"hello": world}"#;
        let meta = test_meta();
        let fallback = serde_json::json!({"fallback": true});
        let output = parse_json_response(raw, meta, fallback.clone());
        assert!(output.parse_error);
        assert_eq!(output.data, fallback);
    }

    fn test_meta() -> AgentMetadata {
        AgentMetadata {
            agent: "test".into(),
            model: "test/model".into(),
            tokens: 100,
            cost: 0.01,
            latency_ms: 50,
        }
    }
}
