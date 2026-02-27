use std::path::{Path, PathBuf};
use std::time::Duration;

use glitchlab_kernel::tool::{ToolCall, ToolCallResult, ToolDefinition, ToolExecutor, ToolPolicy};
use serde_json::json;
use tracing::warn;

/// Maximum number of results returned by `list_files`.
const LIST_FILES_CAP: usize = 500;

/// Maximum chars for `run_command` output (~4,000 tokens).
const RUN_COMMAND_MAX_CHARS: usize = 16_000;

/// Maximum chars for `read_file` output (~12,000 tokens).
const READ_FILE_MAX_CHARS: usize = 48_000;

/// Truncate tool output to `max_chars`, keeping the **tail** (errors and
/// summaries tend to appear at the end). Breaks at a newline boundary and
/// prepends a `[...truncated N chars]` marker.
pub(crate) fn truncate_tool_output(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }

    let skip = s.len() - max_chars;
    // Find the next newline after the skip point so we break cleanly.
    let break_at = s[skip..]
        .find('\n')
        .map(|pos| skip + pos + 1)
        .unwrap_or(skip);

    let truncated_count = break_at;
    format!("[...truncated {truncated_count} chars]\n{}", &s[break_at..])
}

/// The outcome of a tool execution, indicating whether it succeeded or failed,
/// and if it failed, whether the failure is considered retryable.
#[derive(Debug)]
#[allow(dead_code)]
pub enum ToolExecutionOutcome {
    /// The tool executed successfully. The contained `ToolCallResult` has `is_error: false`.
    Success(ToolCallResult),
    /// The tool failed with a transient error that might be resolved by retrying
    /// the operation, possibly with different parameters (e.g., a smaller `read_file` range).
    /// The contained `ToolCallResult` has `is_error: true`.
    RetryableFailure(ToolCallResult),
    /// The tool failed with a permanent error that is unlikely to be resolved by a retry.
    /// The contained `ToolCallResult` has `is_error: true`.
    FatalFailure(ToolCallResult),
}

/// Returns the 5 tool definitions offered to LLMs.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read_file".into(),
            description: "Read the contents of a file relative to the repository root. For large files, use start_line and end_line to read a specific range.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the repository root."
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "Optional 1-based start line number."
                    },
                    "end_line": {
                        "type": "integer",
                        "description": "Optional 1-based end line number (inclusive)."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "write_file".into(),
            description: "Create or overwrite a file.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the repository root."
                    },
                    "content": {
                        "type": "string",
                        "description": "The full content to write to the file."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "edit_file".into(),
            description: "Replace an exact string in a file with new content.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the repository root."
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The exact string to find and replace."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The replacement string."
                    }
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "run_command".into(),
            description: "Execute a shell command (subject to the configured allowlist).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "list_files".into(),
            description: "List files matching a glob pattern relative to the repository root."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match files (e.g. \"src/**/*.rs\")."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        },
    ]
}

/// Dispatches tool calls to concrete implementations, enforcing governance.
pub struct ToolDispatcher {
    working_dir: PathBuf,
    executor: ToolExecutor,
    protected_paths: Vec<String>,
}

impl ToolDispatcher {
    /// Create a new dispatcher rooted at `working_dir`.
    ///
    /// `policy` governs which shell commands `run_command` may execute.
    /// `protected_paths` lists path prefixes that `write_file` / `edit_file` may not touch.
    /// `timeout` bounds shell command execution time.
    pub fn new(
        working_dir: PathBuf,
        policy: ToolPolicy,
        protected_paths: Vec<String>,
        timeout: Duration,
    ) -> Self {
        let executor = ToolExecutor::new(policy, working_dir.clone(), timeout);
        Self {
            working_dir,
            executor,
            protected_paths,
        }
    }

    /// Execute a tool call and return the result. Never panics.
    pub async fn dispatch(&self, call: &ToolCall) -> ToolCallResult {
        let result = match call.name.as_str() {
            "read_file" => self.handle_read_file(&call.input).await,
            "write_file" => self.handle_write_file(&call.input).await,
            "edit_file" => self.handle_edit_file(&call.input).await,
            "run_command" => self.handle_run_command(&call.input).await,
            "list_files" => self.handle_list_files(&call.input),
            unknown => {
                warn!(tool = unknown, "unknown tool requested");
                Err(format!("unknown tool: {unknown}"))
            }
        };

        match result {
            Ok(content) => ToolCallResult {
                tool_call_id: call.id.clone(),
                content,
                is_error: false,
            },
            Err(msg) => ToolCallResult {
                tool_call_id: call.id.clone(),
                content: msg,
                is_error: true,
            },
        }
    }

    // ------------------------------------------------------------------
    // Private handlers
    // ------------------------------------------------------------------

    async fn handle_read_file(&self, input: &serde_json::Value) -> Result<String, String> {
        let path = extract_str(input, "path")?;
        let resolved = self.resolve_path(path)?;
        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|e| format!("failed to read {path}: {e}"))?;

        let start_line = input.get("start_line").and_then(|v| v.as_u64());
        let end_line = input.get("end_line").and_then(|v| v.as_u64());

        let output = match (start_line, end_line) {
            (Some(s), Some(e)) => {
                let s = (s as usize).max(1);
                let e = e as usize;
                let total_lines = content.lines().count();
                let numbered: String = content
                    .lines()
                    .enumerate()
                    .filter(|(i, _)| *i + 1 >= s && *i < e)
                    .map(|(i, l)| format!("{:>4}| {}", i + 1, l))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("[lines {s}-{e} of {total_lines}]\n{numbered}")
            }
            (Some(s), None) => {
                let s = (s as usize).max(1);
                let total_lines = content.lines().count();
                let numbered: String = content
                    .lines()
                    .enumerate()
                    .filter(|(i, _)| *i + 1 >= s)
                    .map(|(i, l)| format!("{:>4}| {}", i + 1, l))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("[lines {s}-{total_lines} of {total_lines}]\n{numbered}")
            }
            _ => content,
        };

        Ok(truncate_tool_output(&output, READ_FILE_MAX_CHARS))
    }

    async fn handle_write_file(&self, input: &serde_json::Value) -> Result<String, String> {
        let path = extract_str(input, "path")?;
        let content = extract_str(input, "content")?;
        self.check_protected(path)?;
        let resolved = self.resolve_path(path)?;

        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("failed to create directories for {path}: {e}"))?;
        }

        tokio::fs::write(&resolved, content)
            .await
            .map_err(|e| format!("failed to write {path}: {e}"))?;

        Ok(format!("wrote {path}"))
    }

    async fn handle_edit_file(&self, input: &serde_json::Value) -> Result<String, String> {
        let path = extract_str(input, "path")?;
        let old_string = extract_str(input, "old_string")?;
        let new_string = extract_str(input, "new_string")?;
        self.check_protected(path)?;
        let resolved = self.resolve_path(path)?;

        let contents = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|e| format!("failed to read {path}: {e}"))?;

        if !contents.contains(old_string) {
            // Provide a hint: show a nearby snippet if the first line matches.
            let first_line = old_string.lines().next().unwrap_or("").trim();
            let hint = if !first_line.is_empty() {
                contents
                    .lines()
                    .enumerate()
                    .find(|(_, l)| l.trim() == first_line)
                    .map(|(i, _)| {
                        let start = i.saturating_sub(1);
                        let end = (i + 4).min(contents.lines().count());
                        let snippet: Vec<&str> =
                            contents.lines().skip(start).take(end - start).collect();
                        format!(
                            ". Hint: line {} has similar content:\n{}",
                            start + 1,
                            snippet.join("\n")
                        )
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };
            return Err(format!("old_string not found in {path}{hint}"));
        }

        let new_contents = contents.replacen(old_string, new_string, 1);
        tokio::fs::write(&resolved, new_contents)
            .await
            .map_err(|e| format!("failed to write {path}: {e}"))?;

        Ok(format!("edited {path}"))
    }

    async fn handle_run_command(&self, input: &serde_json::Value) -> Result<String, String> {
        let command = extract_str(input, "command")?;
        match self.executor.execute(command).await {
            Ok(result) => {
                let mut output = String::new();
                if !result.stdout.is_empty() {
                    output.push_str(&result.stdout);
                }
                if !result.stderr.is_empty() {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str("stderr:\n");
                    output.push_str(&result.stderr);
                }
                output.push_str(&format!("\nexit code: {}", result.returncode));
                Ok(truncate_tool_output(&output, RUN_COMMAND_MAX_CHARS))
            }
            Err(e) => Err(format!("command rejected: {e}")),
        }
    }

    fn handle_list_files(&self, input: &serde_json::Value) -> Result<String, String> {
        let pattern = extract_str(input, "pattern")?;
        let full_pattern = self.working_dir.join(pattern);
        let full_pattern_str = full_pattern.to_string_lossy();

        let entries =
            glob::glob(&full_pattern_str).map_err(|e| format!("invalid glob pattern: {e}"))?;

        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                Ok(p) => {
                    if let Ok(rel) = p.strip_prefix(&self.working_dir) {
                        paths.push(rel.to_string_lossy().into_owned());
                    }
                }
                Err(e) => {
                    warn!(error = %e, "glob entry error");
                }
            }
            if paths.len() >= LIST_FILES_CAP {
                break;
            }
        }

        if paths.is_empty() {
            Ok("no files matched".into())
        } else {
            Ok(paths.join("\n"))
        }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Resolve a relative path against working_dir, rejecting traversal.
    fn resolve_path(&self, path: &str) -> Result<PathBuf, String> {
        let joined = self.working_dir.join(path);

        // Canonicalize both to detect traversal (e.g. `../../etc/passwd`).
        // The target file may not exist yet (write_file), so we canonicalize
        // the deepest existing ancestor.
        let canonical_base = self
            .working_dir
            .canonicalize()
            .map_err(|e| format!("cannot canonicalize working dir: {e}"))?;

        let canonical_target = canonicalize_best_effort(&joined)?;

        if !canonical_target.starts_with(&canonical_base) {
            return Err(format!("path traversal rejected: {path}"));
        }

        Ok(joined)
    }

    /// Check if a path is protected.
    fn check_protected(&self, path: &str) -> Result<(), String> {
        for protected in &self.protected_paths {
            if path.starts_with(protected.as_str()) {
                return Err(format!(
                    "PROTECTED PATH — cannot modify '{}'. \
                     This path is protected by project policy. \
                     Do NOT retry this operation.",
                    path
                ));
            }
        }
        Ok(())
    }
}

/// Canonicalize a path, falling back to canonicalizing the deepest existing
/// ancestor and appending the remaining components. This handles paths where
/// the leaf file doesn't exist yet (e.g. `write_file` creating a new file).
fn canonicalize_best_effort(path: &Path) -> Result<PathBuf, String> {
    if let Ok(c) = path.canonicalize() {
        return Ok(c);
    }

    // Walk up until we find an ancestor that exists.
    let mut existing = path.to_path_buf();
    let mut remainder = Vec::new();
    loop {
        if existing.exists() {
            let base = existing
                .canonicalize()
                .map_err(|e| format!("cannot canonicalize {}: {e}", existing.display()))?;
            let mut result = base;
            for part in remainder.into_iter().rev() {
                result.push(part);
            }
            return Ok(result);
        }
        if let Some(name) = existing.file_name() {
            remainder.push(name.to_os_string());
        }
        if !existing.pop() {
            break;
        }
    }

    Err(format!("cannot resolve path: {}", path.display()))
}

/// Extract a required string field from a JSON object.
fn extract_str<'a>(input: &'a serde_json::Value, field: &str) -> Result<&'a str, String> {
    input
        .get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing or invalid argument: {field}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_policy() -> ToolPolicy {
        ToolPolicy::new(
            vec!["echo".into(), "cat".into(), "ls".into()],
            vec!["rm".into()],
        )
    }

    fn make_dispatcher(dir: &Path) -> ToolDispatcher {
        ToolDispatcher::new(
            dir.to_path_buf(),
            test_policy(),
            vec![".env".into(), "secrets/".into()],
            Duration::from_secs(10),
        )
    }

    fn make_call(name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: name.into(),
            input,
        }
    }

    // -- truncate_tool_output --

    #[test]
    fn truncate_tool_output_short_noop() {
        let input = "hello world";
        assert_eq!(truncate_tool_output(input, 100), input);
    }

    #[test]
    fn truncate_tool_output_long_keeps_tail() {
        let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let input = lines.join("\n");
        let result = truncate_tool_output(&input, 200);
        assert!(result.starts_with("[...truncated"));
        assert!(result.contains("chars]"));
        // The tail (last lines) should be preserved.
        assert!(result.contains("line 99"));
        assert!(result.len() <= 200 + 40); // output + marker overhead
    }

    #[test]
    fn truncate_tool_output_breaks_at_newline() {
        let input = "aaaa\nbbbb\ncccc\ndddd\neeee";
        let result = truncate_tool_output(input, 15);
        assert!(result.starts_with("[...truncated"));
        // Should not start mid-line after the marker.
        let after_marker = result.split_once("]\n").unwrap().1;
        // The kept portion should start at a line boundary.
        assert!(
            after_marker.starts_with("cccc")
                || after_marker.starts_with("dddd")
                || after_marker.starts_with("eeee")
        );
    }

    // -- tool_definitions --

    #[test]
    fn tool_definitions_has_five_tools() {
        let defs = tool_definitions();
        assert_eq!(defs.len(), 5);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"run_command"));
        assert!(names.contains(&"list_files"));
    }

    #[test]
    fn tool_definitions_schemas_are_valid() {
        for def in tool_definitions() {
            assert_eq!(def.input_schema["type"], "object");
            assert!(def.input_schema["properties"].is_object());
            assert!(def.input_schema["required"].is_array());
            assert!(!def.description.is_empty());
        }
    }

    // -- dispatch unknown / missing args --

    #[tokio::test]
    async fn dispatch_unknown_tool() {
        let dir = TempDir::new().unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call("does_not_exist", json!({}));
        let result = dispatcher.dispatch(&call).await;
        assert!(result.is_error);
        assert!(result.content.contains("unknown tool"));
    }

    #[tokio::test]
    async fn dispatch_missing_args() {
        let dir = TempDir::new().unwrap();
        let dispatcher = make_dispatcher(dir.path());
        // read_file without path
        let call = make_call("read_file", json!({}));
        let result = dispatcher.dispatch(&call).await;
        assert!(result.is_error);
        assert!(result.content.contains("missing or invalid argument"));
    }

    // -- read_file --

    #[tokio::test]
    async fn read_file_success() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hello world").unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call("read_file", json!({"path": "hello.txt"}));
        let result = dispatcher.dispatch(&call).await;
        assert!(!result.is_error);
        assert_eq!(result.content, "hello world");
    }

    #[tokio::test]
    async fn read_file_line_range() {
        let dir = TempDir::new().unwrap();
        let content = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("lines.txt"), &content).unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call(
            "read_file",
            json!({"path": "lines.txt", "start_line": 3, "end_line": 5}),
        );
        let result = dispatcher.dispatch(&call).await;
        assert!(!result.is_error);
        assert!(result.content.contains("line 3"));
        assert!(result.content.contains("line 5"));
        assert!(!result.content.contains("line 2"));
        assert!(!result.content.contains("line 6"));
        assert!(result.content.contains("[lines 3-5 of 10]"));
    }

    #[tokio::test]
    async fn read_file_not_found() {
        let dir = TempDir::new().unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call("read_file", json!({"path": "nope.txt"}));
        let result = dispatcher.dispatch(&call).await;
        assert!(result.is_error);
        assert!(result.content.contains("failed to read"));
    }

    #[tokio::test]
    async fn read_file_path_traversal_rejected() {
        let dir = TempDir::new().unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call("read_file", json!({"path": "../../etc/passwd"}));
        let result = dispatcher.dispatch(&call).await;
        assert!(result.is_error);
        assert!(result.content.contains("path traversal rejected"));
    }

    // -- write_file --

    #[tokio::test]
    async fn write_file_success() {
        let dir = TempDir::new().unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call("write_file", json!({"path": "out.txt", "content": "data"}));
        let result = dispatcher.dispatch(&call).await;
        assert!(!result.is_error);
        assert!(result.content.contains("wrote"));
        let contents = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
        assert_eq!(contents, "data");
    }

    #[tokio::test]
    async fn write_file_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call(
            "write_file",
            json!({"path": "a/b/c.txt", "content": "nested"}),
        );
        let result = dispatcher.dispatch(&call).await;
        assert!(!result.is_error);
        let contents = std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap();
        assert_eq!(contents, "nested");
    }

    #[tokio::test]
    async fn write_file_protected_path_rejected() {
        let dir = TempDir::new().unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call(
            "write_file",
            json!({"path": ".env", "content": "SECRET=oops"}),
        );
        let result = dispatcher.dispatch(&call).await;
        assert!(result.is_error);
        assert!(result.content.contains("PROTECTED PATH"));
    }

    // -- edit_file --

    #[tokio::test]
    async fn edit_file_success() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("code.rs"), "fn old() {}").unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call(
            "edit_file",
            json!({"path": "code.rs", "old_string": "old", "new_string": "new"}),
        );
        let result = dispatcher.dispatch(&call).await;
        assert!(!result.is_error);
        let contents = std::fs::read_to_string(dir.path().join("code.rs")).unwrap();
        assert_eq!(contents, "fn new() {}");
    }

    #[tokio::test]
    async fn edit_file_string_not_found() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("code.rs"), "fn main() {}").unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call(
            "edit_file",
            json!({"path": "code.rs", "old_string": "nonexistent", "new_string": "x"}),
        );
        let result = dispatcher.dispatch(&call).await;
        assert!(result.is_error);
        assert!(result.content.contains("old_string not found"));
    }

    #[tokio::test]
    async fn edit_file_protected_path_rejected() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("secrets"), "").unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call(
            "edit_file",
            json!({"path": "secrets/key.pem", "old_string": "a", "new_string": "b"}),
        );
        let result = dispatcher.dispatch(&call).await;
        assert!(result.is_error);
        assert!(result.content.contains("PROTECTED PATH"));
    }

    // -- run_command --

    #[tokio::test]
    async fn run_command_success() {
        let dir = TempDir::new().unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call("run_command", json!({"command": "echo hello"}));
        let result = dispatcher.dispatch(&call).await;
        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
        assert!(result.content.contains("exit code: 0"));
    }

    #[tokio::test]
    async fn run_command_blocked_by_policy() {
        let dir = TempDir::new().unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call("run_command", json!({"command": "rm -rf /"}));
        let result = dispatcher.dispatch(&call).await;
        assert!(result.is_error);
        assert!(result.content.contains("command rejected"));
    }

    // -- list_files --

    #[tokio::test]
    async fn list_files_matches() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();

        let dispatcher = make_dispatcher(dir.path());
        let call = make_call("list_files", json!({"pattern": "src/*.rs"}));
        let result = dispatcher.dispatch(&call).await;
        assert!(!result.is_error);
        assert!(result.content.contains("src/main.rs"));
        assert!(result.content.contains("src/lib.rs"));
        assert!(!result.content.contains("README.md"));
    }

    #[tokio::test]
    async fn list_files_no_matches() {
        let dir = TempDir::new().unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call("list_files", json!({"pattern": "*.xyz"}));
        let result = dispatcher.dispatch(&call).await;
        assert!(!result.is_error);
        assert_eq!(result.content, "no files matched");
    }

    #[tokio::test]
    async fn read_file_start_line_only() {
        let dir = TempDir::new().unwrap();
        let content = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("lines.txt"), &content).unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call("read_file", json!({"path": "lines.txt", "start_line": 8}));
        let result = dispatcher.dispatch(&call).await;
        assert!(!result.is_error);
        assert!(result.content.contains("line 8"));
        assert!(result.content.contains("line 10"));
        assert!(!result.content.contains("line 7"));
        assert!(result.content.contains("[lines 8-10 of 10]"));
    }

    #[tokio::test]
    async fn edit_file_not_found_with_hint() {
        let dir = TempDir::new().unwrap();
        let content = "fn main() {\n    println!(\"hello\");\n    return;\n}\n";
        std::fs::write(dir.path().join("code.rs"), content).unwrap();
        let dispatcher = make_dispatcher(dir.path());
        // First line matches but full string doesn't → hint shown.
        let call = make_call(
            "edit_file",
            json!({"path": "code.rs", "old_string": "fn main() {\n    DIFFERENT", "new_string": "x"}),
        );
        let result = dispatcher.dispatch(&call).await;
        assert!(result.is_error);
        assert!(result.content.contains("old_string not found"));
        assert!(result.content.contains("Hint: line"));
    }

    #[tokio::test]
    async fn check_protected_error_message_format() {
        let dir = TempDir::new().unwrap();
        let dispatcher = make_dispatcher(dir.path());
        let call = make_call(
            "write_file",
            json!({"path": ".env", "content": "SECRET=oops"}),
        );
        let result = dispatcher.dispatch(&call).await;
        assert!(result.is_error);
        assert!(result.content.contains("PROTECTED PATH"));
        assert!(result.content.contains("Do NOT retry"));
    }
}
