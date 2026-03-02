use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// ToolPolicy — what an org is allowed to execute
// ---------------------------------------------------------------------------

/// Defines the tool execution policy for an org or agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPolicy {
    /// Commands that are allowed (prefix-matched).
    /// A command must start with one of these prefixes to be permitted.
    pub allowed: Vec<String>,

    /// Patterns that are always blocked, regardless of the allowlist.
    /// Checked first — a blocked command is never allowed.
    pub blocked: Vec<String>,
}

impl ToolPolicy {
    pub fn new(allowed: Vec<String>, blocked: Vec<String>) -> Self {
        Self { allowed, blocked }
    }

    /// Check whether a command is permitted under this policy.
    ///
    /// Handles chained commands (`cmd1 && cmd2`, `cmd1 ; cmd2`, `cmd1 || cmd2`):
    /// each segment is checked independently. `cd` is always allowed (directory
    /// change is a harmless shell builtin).
    pub fn check(&self, command: &str) -> std::result::Result<(), String> {
        let trimmed = command.trim();

        // Blocklist takes priority — checked against the FULL command string.
        for pattern in &self.blocked {
            if trimmed.contains(pattern.as_str()) {
                return Err(format!("blocked by pattern `{pattern}`"));
            }
        }

        // Split on shell operators and check each segment.
        for segment in split_shell_chain(trimmed) {
            let seg = segment.trim();
            if seg.is_empty() {
                continue;
            }
            // `cd` is always allowed (harmless shell builtin).
            if seg == "cd" || seg.starts_with("cd ") {
                continue;
            }
            let mut matched = false;
            for prefix in &self.allowed {
                if seg.starts_with(prefix.as_str()) {
                    matched = true;
                    break;
                }
            }
            if !matched {
                return Err(format!("no matching allowlist entry for `{seg}`"));
            }
        }

        Ok(())
    }
}

/// Split a shell command on `&&`, `||`, and `;` into individual segments.
fn split_shell_chain(command: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let bytes = command.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b';' {
            segments.push(&command[start..i]);
            start = i + 1;
        } else if i + 1 < len && bytes[i] == b'&' && bytes[i + 1] == b'&' {
            segments.push(&command[start..i]);
            start = i + 2;
            i += 1; // skip second '&'
        } else if i + 1 < len && bytes[i] == b'|' && bytes[i + 1] == b'|' {
            segments.push(&command[start..i]);
            start = i + 2;
            i += 1; // skip second '|'
        }
        i += 1;
    }
    segments.push(&command[start..]);
    segments
}

// ---------------------------------------------------------------------------
// ToolDefinition — schema for a tool offered to an LLM
// ---------------------------------------------------------------------------

/// Definition of a tool that can be offered to an LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name (e.g. "read_file", "run_command").
    pub name: String,
    /// Human-readable description for the LLM.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub input_schema: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Minimal tool parameter schemas
// ---------------------------------------------------------------------------

/// Parameters for the `read_file` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReadFileParams {
    /// Absolute or workspace-relative path of the file to read.
    pub path: String,
}

/// Parameters for the `write_file` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WriteFileParams {
    /// Absolute or workspace-relative path of the file to write.
    pub path: String,
    /// Full content to write to the file (overwrites existing content).
    pub content: String,
}

/// Parameters for the `edit_file` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EditFileParams {
    /// Absolute or workspace-relative path of the file to edit.
    pub path: String,
    /// Exact string to search for in the file (must be unique).
    pub old_str: String,
    /// Replacement string that will replace `old_str`.
    pub new_str: String,
}

/// Parameters for the `run_command` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunCommandParams {
    /// Shell command to execute within the sandboxed working directory.
    pub command: String,
}

fn default_list_files_path() -> String {
    ".".into()
}

/// Parameters for the `list_files` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ListFilesParams {
    /// Directory path to list. Defaults to `"."` (workspace root).
    #[serde(default = "default_list_files_path")]
    pub path: String,
    /// Whether to list files recursively. Defaults to `false`.
    #[serde(default)]
    pub recursive: bool,
}

/// Returns the minimal set of `ToolDefinition` objects required for code editing:
/// `read_file`, `write_file`, `edit_file`, `run_command`, and `list_files`.
///
/// The `input_schema` for each tool is generated from the corresponding
/// parameter struct using [`schemars`].
pub fn get_minimal_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read_file".into(),
            description: "Read the contents of a file from the workspace. \
                Returns the full text content of the specified file."
                .into(),
            input_schema: serde_json::to_value(schemars::schema_for!(ReadFileParams))
                .expect("ReadFileParams schema is always serializable"),
        },
        ToolDefinition {
            name: "write_file".into(),
            description: "Write content to a file, creating it if it does not exist \
                or overwriting it if it does."
                .into(),
            input_schema: serde_json::to_value(schemars::schema_for!(WriteFileParams))
                .expect("WriteFileParams schema is always serializable"),
        },
        ToolDefinition {
            name: "edit_file".into(),
            description: "Replace an exact substring in a file with new text. \
                The `old_str` must appear exactly once in the file."
                .into(),
            input_schema: serde_json::to_value(schemars::schema_for!(EditFileParams))
                .expect("EditFileParams schema is always serializable"),
        },
        ToolDefinition {
            name: "run_command".into(),
            description: "Execute a shell command in the sandboxed working directory. \
                Returns stdout, stderr, and the exit code."
                .into(),
            input_schema: serde_json::to_value(schemars::schema_for!(RunCommandParams))
                .expect("RunCommandParams schema is always serializable"),
        },
        ToolDefinition {
            name: "list_files".into(),
            description: "List files in a directory. Optionally recurse into subdirectories."
                .into(),
            input_schema: serde_json::to_value(schemars::schema_for!(ListFilesParams))
                .expect("ListFilesParams schema is always serializable"),
        },
    ]
}

// ---------------------------------------------------------------------------
// ToolCall — a tool invocation requested by the LLM
// ---------------------------------------------------------------------------

/// A tool call requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique ID for this call (provider-assigned, used to correlate results).
    pub id: String,
    /// Name of the tool to invoke.
    pub name: String,
    /// Arguments as a JSON object.
    pub input: serde_json::Value,
}

// ---------------------------------------------------------------------------
// ToolCallResult — result sent back to the LLM
// ---------------------------------------------------------------------------

/// Result of executing a tool call, sent back to the LLM.
///
/// This is the *conversation-level* result — a simplified representation of
/// what happened. The existing `ToolResult` is the *execution-level* result
/// (command stdout/stderr/returncode). A `ToolResult` from
/// `ToolExecutor::execute()` gets condensed into a `ToolCallResult` by the
/// agent loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    /// ID of the `ToolCall` this is responding to.
    pub tool_call_id: String,
    /// Output content (stdout, file contents, error message, etc.).
    pub content: String,
    /// True if the tool execution failed.
    #[serde(default)]
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// ToolResult — output from a tool execution
// ---------------------------------------------------------------------------

/// Result of a sandboxed command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// The command that was executed.
    pub command: String,

    /// Standard output.
    pub stdout: String,

    /// Standard error.
    pub stderr: String,

    /// Process exit code.
    pub returncode: i32,
}

impl ToolResult {
    /// True if the command exited with code 0.
    pub fn success(&self) -> bool {
        self.returncode == 0
    }
}

// ---------------------------------------------------------------------------
// ToolExecutor — sandboxed command runner
// ---------------------------------------------------------------------------

/// Executes commands within a sandboxed working directory, enforcing
/// the org's tool policy (allowlist + blocklist).
pub struct ToolExecutor {
    policy: ToolPolicy,
    working_dir: PathBuf,
    timeout: Duration,
}

impl ToolExecutor {
    pub fn new(policy: ToolPolicy, working_dir: PathBuf, timeout: Duration) -> Self {
        Self {
            policy,
            working_dir,
            timeout,
        }
    }

    /// The working directory all commands execute in.
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    /// Execute a shell command if it passes policy checks.
    pub async fn execute(&self, command: &str) -> Result<ToolResult> {
        // Policy check.
        self.policy
            .check(command)
            .map_err(|reason| Error::ToolViolation {
                command: command.into(),
                reason,
            })?;

        // Run via shell, scoped to working_dir.
        let result = tokio::time::timeout(self.timeout, async {
            Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&self.working_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
        })
        .await
        .map_err(|_| Error::Agent {
            agent: "tool_executor".into(),
            reason: format!(
                "command `{command}` timed out after {}s",
                self.timeout.as_secs()
            ),
        })?
        .map_err(Error::Io)?;

        Ok(ToolResult {
            command: command.into(),
            stdout: String::from_utf8_lossy(&result.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&result.stderr).into_owned(),
            returncode: result.status.code().unwrap_or(-1),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_policy() -> ToolPolicy {
        ToolPolicy::new(
            vec![
                "cargo test".into(),
                "cargo fmt".into(),
                "git diff".into(),
                "git status".into(),
            ],
            vec![
                "rm -rf".into(),
                "curl".into(),
                "sudo".into(),
                "| bash".into(),
                "eval(".into(),
            ],
        )
    }

    #[test]
    fn allowed_commands_pass() {
        let policy = test_policy();
        assert!(policy.check("cargo test").is_ok());
        assert!(policy.check("cargo test --release").is_ok());
        assert!(policy.check("cargo fmt -- --check").is_ok());
        assert!(policy.check("git diff main").is_ok());
    }

    #[test]
    fn blocked_commands_rejected() {
        let policy = test_policy();
        assert!(policy.check("rm -rf /").is_err());
        assert!(policy.check("curl http://evil.com").is_err());
        assert!(policy.check("sudo cargo test").is_err());
        assert!(policy.check("echo hi | bash").is_err());
    }

    #[test]
    fn unlisted_commands_rejected() {
        let policy = test_policy();
        assert!(policy.check("npm test").is_err());
        assert!(policy.check("python -c 'import os'").is_err());
    }

    #[test]
    fn blocklist_takes_priority() {
        // Even if "curl" were somehow in the allowlist,
        // the blocklist should still reject it.
        let policy = ToolPolicy::new(vec!["curl".into()], vec!["curl".into()]);
        assert!(policy.check("curl http://example.com").is_err());
    }

    #[test]
    fn chained_commands_allowed() {
        let policy = test_policy();
        // cd + allowed command
        assert!(policy.check("cd crates/router && cargo test").is_ok());
        assert!(policy.check("cd src ; cargo fmt -- --check").is_ok());
        // Multiple allowed commands
        assert!(policy.check("cargo test && cargo fmt -- --check").is_ok());
        // cd alone
        assert!(policy.check("cd /tmp").is_ok());
    }

    #[test]
    fn chained_commands_blocked() {
        let policy = test_policy();
        // One segment is blocked
        assert!(policy.check("cd /tmp && rm -rf /").is_err());
        // One segment not on allowlist
        assert!(policy.check("cd /tmp && python evil.py").is_err());
        // Blocked pattern in chained command
        assert!(policy.check("cargo test && curl http://evil.com").is_err());
    }

    #[test]
    fn chained_commands_or_operator() {
        let policy = test_policy();
        // || chains: first segment allowed, second blocked → rejected
        assert!(policy.check("cargo test || rm -rf /").is_err());
        // || chains: both segments allowed
        assert!(policy.check("cargo test || cargo fmt").is_ok());
    }

    #[test]
    fn tool_result_success_and_failure() {
        let ok = ToolResult {
            command: "echo hi".into(),
            stdout: "hi\n".into(),
            stderr: String::new(),
            returncode: 0,
        };
        assert!(ok.success());

        let fail = ToolResult {
            command: "false".into(),
            stdout: String::new(),
            stderr: "error".into(),
            returncode: 1,
        };
        assert!(!fail.success());
    }

    #[test]
    fn executor_working_dir() {
        let dir = PathBuf::from("/tmp");
        let executor = ToolExecutor::new(test_policy(), dir.clone(), Duration::from_secs(10));
        assert_eq!(executor.working_dir(), dir);
    }

    #[tokio::test]
    async fn execute_allowed_succeeds() {
        let dir = std::env::temp_dir();
        let policy = ToolPolicy::new(vec!["echo".into()], vec![]);
        let executor = ToolExecutor::new(policy, dir, Duration::from_secs(10));
        let result = executor.execute("echo hello world").await.unwrap();
        assert_eq!(result.stdout.trim(), "hello world");
        assert!(result.success());
        assert_eq!(result.command, "echo hello world");
    }

    #[tokio::test]
    async fn execute_blocked_fails() {
        let dir = std::env::temp_dir();
        let executor = ToolExecutor::new(test_policy(), dir, Duration::from_secs(10));
        let result = executor.execute("rm -rf /tmp/foo").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_unlisted_fails() {
        let dir = std::env::temp_dir();
        let executor = ToolExecutor::new(test_policy(), dir, Duration::from_secs(10));
        let result = executor.execute("python -c 'print(1)'").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_nonzero_exit() {
        let dir = std::env::temp_dir();
        let policy = ToolPolicy::new(vec!["sh".into()], vec![]);
        let executor = ToolExecutor::new(policy, dir, Duration::from_secs(10));
        let result = executor.execute("sh -c 'exit 42'").await.unwrap();
        assert!(!result.success());
        assert_eq!(result.returncode, 42);
    }

    #[tokio::test]
    async fn execute_captures_stderr() {
        let dir = std::env::temp_dir();
        let policy = ToolPolicy::new(vec!["sh".into()], vec![]);
        let executor = ToolExecutor::new(policy, dir, Duration::from_secs(10));
        let result = executor.execute("sh -c 'echo err >&2'").await.unwrap();
        assert!(result.stderr.contains("err"));
    }

    #[test]
    fn tool_definition_serde_roundtrip() {
        let def = ToolDefinition {
            name: "read_file".into(),
            description: "Read a file from disk".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "required": ["path"]
            }),
        };
        let json = serde_json::to_string(&def).unwrap();
        let parsed: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "read_file");
        assert_eq!(parsed.description, "Read a file from disk");
        assert!(
            parsed.input_schema["properties"]["path"]["type"]
                .as_str()
                .unwrap()
                .contains("string")
        );
    }

    #[test]
    fn tool_call_serde_roundtrip() {
        let call = ToolCall {
            id: "call_123".into(),
            name: "run_command".into(),
            input: serde_json::json!({"command": "cargo test"}),
        };
        let json = serde_json::to_string(&call).unwrap();
        let parsed: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "call_123");
        assert_eq!(parsed.name, "run_command");
        assert_eq!(parsed.input["command"], "cargo test");
    }

    #[test]
    fn tool_call_result_serde_default_is_error() {
        let json = r#"{"tool_call_id": "call_1", "content": "file contents here"}"#;
        let result: ToolCallResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.tool_call_id, "call_1");
        assert_eq!(result.content, "file contents here");
        assert!(!result.is_error);
    }

    #[test]
    fn tool_call_result_with_error() {
        let result = ToolCallResult {
            tool_call_id: "call_2".into(),
            content: "command failed: exit code 1".into(),
            is_error: true,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: ToolCallResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_error);
        assert_eq!(parsed.tool_call_id, "call_2");
        assert!(parsed.content.contains("exit code 1"));
    }

    // -----------------------------------------------------------------------
    // get_minimal_tool_definitions() tests
    // -----------------------------------------------------------------------

    #[test]
    fn minimal_tool_definitions_returns_five_tools() {
        let defs = get_minimal_tool_definitions();
        assert_eq!(defs.len(), 5);
    }

    #[test]
    fn minimal_tool_definitions_names_are_correct() {
        let defs = get_minimal_tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"run_command"));
        assert!(names.contains(&"list_files"));
    }

    #[test]
    fn minimal_tool_definitions_have_descriptions() {
        let defs = get_minimal_tool_definitions();
        for def in &defs {
            assert!(
                !def.description.is_empty(),
                "tool `{}` has empty description",
                def.name
            );
        }
    }

    #[test]
    fn minimal_tool_definitions_schemas_are_objects() {
        let defs = get_minimal_tool_definitions();
        for def in &defs {
            assert!(
                def.input_schema.is_object(),
                "tool `{}` schema is not a JSON object",
                def.name
            );
        }
    }

    #[test]
    fn read_file_schema_requires_path() {
        let defs = get_minimal_tool_definitions();
        let read_def = defs.iter().find(|d| d.name == "read_file").unwrap();
        let schema = &read_def.input_schema;
        // The schema should have a "properties" object containing "path"
        assert!(
            schema["properties"]["path"].is_object(),
            "read_file schema missing 'path' property"
        );
    }

    #[test]
    fn write_file_schema_requires_path_and_content() {
        let defs = get_minimal_tool_definitions();
        let def = defs.iter().find(|d| d.name == "write_file").unwrap();
        let schema = &def.input_schema;
        assert!(
            schema["properties"]["path"].is_object(),
            "write_file schema missing 'path' property"
        );
        assert!(
            schema["properties"]["content"].is_object(),
            "write_file schema missing 'content' property"
        );
    }

    #[test]
    fn edit_file_schema_has_required_fields() {
        let defs = get_minimal_tool_definitions();
        let def = defs.iter().find(|d| d.name == "edit_file").unwrap();
        let schema = &def.input_schema;
        assert!(
            schema["properties"]["path"].is_object(),
            "edit_file schema missing 'path' property"
        );
        assert!(
            schema["properties"]["old_str"].is_object(),
            "edit_file schema missing 'old_str' property"
        );
        assert!(
            schema["properties"]["new_str"].is_object(),
            "edit_file schema missing 'new_str' property"
        );
    }

    #[test]
    fn run_command_schema_has_command_field() {
        let defs = get_minimal_tool_definitions();
        let def = defs.iter().find(|d| d.name == "run_command").unwrap();
        let schema = &def.input_schema;
        assert!(
            schema["properties"]["command"].is_object(),
            "run_command schema missing 'command' property"
        );
    }

    #[test]
    fn list_files_schema_has_path_field() {
        let defs = get_minimal_tool_definitions();
        let def = defs.iter().find(|d| d.name == "list_files").unwrap();
        let schema = &def.input_schema;
        assert!(
            schema["properties"]["path"].is_object(),
            "list_files schema missing 'path' property"
        );
    }

    #[test]
    fn read_file_params_serde_roundtrip() {
        let params = ReadFileParams {
            path: "src/main.rs".into(),
        };
        let json = serde_json::to_string(&params).unwrap();
        let parsed: ReadFileParams = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.path, "src/main.rs");
    }

    #[test]
    fn write_file_params_serde_roundtrip() {
        let params = WriteFileParams {
            path: "out.txt".into(),
            content: "hello".into(),
        };
        let json = serde_json::to_string(&params).unwrap();
        let parsed: WriteFileParams = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.path, "out.txt");
        assert_eq!(parsed.content, "hello");
    }

    #[test]
    fn edit_file_params_serde_roundtrip() {
        let params = EditFileParams {
            path: "lib.rs".into(),
            old_str: "foo".into(),
            new_str: "bar".into(),
        };
        let json = serde_json::to_string(&params).unwrap();
        let parsed: EditFileParams = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.path, "lib.rs");
        assert_eq!(parsed.old_str, "foo");
        assert_eq!(parsed.new_str, "bar");
    }

    #[test]
    fn run_command_params_serde_roundtrip() {
        let params = RunCommandParams {
            command: "cargo test".into(),
        };
        let json = serde_json::to_string(&params).unwrap();
        let parsed: RunCommandParams = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.command, "cargo test");
    }

    #[test]
    fn list_files_params_defaults() {
        let params: ListFilesParams = serde_json::from_str("{}").unwrap();
        assert_eq!(params.path, ".");
        assert!(!params.recursive);
    }

    #[test]
    fn list_files_params_serde_roundtrip() {
        let params = ListFilesParams {
            path: "src/".into(),
            recursive: true,
        };
        let json = serde_json::to_string(&params).unwrap();
        let parsed: ListFilesParams = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.path, "src/");
        assert!(parsed.recursive);
    }
}
