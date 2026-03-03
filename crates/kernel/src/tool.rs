use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Tool trait — callable unit offered to an agent
// ---------------------------------------------------------------------------

/// A tool that an agent can invoke.
///
/// Implementations encapsulate the logic for a specific capability
/// (e.g. reading a file, running a command, querying a database).
/// The framework calls `definition()` to advertise the tool to an LLM
/// and `call()` to execute it.
pub trait Tool: Send + Sync {
    /// The tool's definition, advertised to the LLM.
    fn definition(&self) -> &ToolDefinition;

    /// Execute the tool with the given call arguments.
    fn call(&self, call: ToolCall) -> impl Future<Output = Result<ToolCallResult>> + Send;
}

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

    /// Tool names this policy permits the agent to invoke.
    ///
    /// When empty, all tool names are permitted (backward-compatible default).
    /// When non-empty, only the listed tool names may be dispatched; any other
    /// tool call is rejected with a `ToolViolation` before execution begins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permitted_tools: Vec<String>,
}

impl ToolPolicy {
    pub fn new(allowed: Vec<String>, blocked: Vec<String>) -> Self {
        Self {
            allowed,
            blocked,
            permitted_tools: Vec::new(),
        }
    }

    /// Builder: restrict which tool names this policy permits.
    ///
    /// Replaces any previously set list. An empty list means *all* tools are
    /// permitted (the default). Pass a non-empty list to restrict the agent.
    pub fn with_permitted_tools(mut self, tools: Vec<String>) -> Self {
        self.permitted_tools = tools;
        self
    }

    /// Check whether invoking a tool by `name` is permitted under this policy.
    ///
    /// Returns `Ok(())` if the tool is permitted, or `Err(reason)` if it is not.
    /// When `permitted_tools` is empty every tool name is allowed (open default).
    pub fn check_tool(&self, name: &str) -> std::result::Result<(), String> {
        if self.permitted_tools.is_empty() {
            return Ok(());
        }
        if self.permitted_tools.iter().any(|t| t == name) {
            Ok(())
        } else {
            Err(format!(
                "tool `{name}` is not permitted; allowed tools: [{}]",
                self.permitted_tools.join(", ")
            ))
        }
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

    /// Check whether a tool name is permitted under this executor's policy.
    ///
    /// Delegates to [`ToolPolicy::check_tool`]. Call this before dispatching
    /// any tool invocation to enforce per-agent tool permissions.
    pub fn check_tool(&self, tool_name: &str) -> std::result::Result<(), String> {
        self.policy.check_tool(tool_name)
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
    async fn execute_timeout_produces_error() {
        let dir = std::env::temp_dir();
        let policy = ToolPolicy::new(vec!["sleep".into()], vec![]);
        let executor = ToolExecutor::new(policy, dir, Duration::from_millis(50));
        let result = executor.execute("sleep 10").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("timed out"),
            "expected timeout error, got: {err}"
        );
    }

    #[tokio::test]
    async fn execute_captures_stderr() {
        let dir = std::env::temp_dir();
        let policy = ToolPolicy::new(vec!["sh".into()], vec![]);
        let executor = ToolExecutor::new(policy, dir, Duration::from_secs(10));
        let result = executor.execute("sh -c 'echo err >&2'").await.unwrap();
        assert!(result.stderr.contains("err"));
    }

    // -- check_tool (permitted_tools) --

    #[test]
    fn check_tool_empty_permitted_allows_all() {
        // When permitted_tools is empty, all tool names are allowed.
        let policy = ToolPolicy::new(vec![], vec![]);
        assert!(policy.check_tool("read_file").is_ok());
        assert!(policy.check_tool("write_file").is_ok());
        assert!(policy.check_tool("run_command").is_ok());
        assert!(policy.check_tool("list_files").is_ok());
        assert!(policy.check_tool("edit_file").is_ok());
    }

    #[test]
    fn check_tool_permitted_list_allows_listed_tools() {
        let policy = ToolPolicy::new(vec![], vec![])
            .with_permitted_tools(vec!["read_file".into(), "list_files".into()]);
        assert!(policy.check_tool("read_file").is_ok());
        assert!(policy.check_tool("list_files").is_ok());
    }

    #[test]
    fn check_tool_permitted_list_rejects_unlisted_tools() {
        let policy = ToolPolicy::new(vec![], vec![]).with_permitted_tools(vec!["read_file".into()]);
        let result = policy.check_tool("write_file");
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(
            msg.contains("write_file"),
            "error should name the rejected tool: {msg}"
        );
    }

    #[test]
    fn check_tool_permitted_list_rejects_run_command_when_not_listed() {
        let policy = ToolPolicy::new(vec!["cargo".into()], vec![])
            .with_permitted_tools(vec!["read_file".into()]);
        // Even though cargo is in the command allowlist, run_command itself
        // is not in permitted_tools.
        let result = policy.check_tool("run_command");
        assert!(result.is_err());
    }

    #[test]
    fn with_permitted_tools_builder_returns_self() {
        let policy = ToolPolicy::new(vec!["cargo".into()], vec!["rm".into()])
            .with_permitted_tools(vec!["read_file".into(), "run_command".into()]);
        // Original command policy is unchanged.
        assert!(policy.check("cargo test").is_ok());
        assert!(policy.check("rm -rf /").is_err());
        // Tool permission check works.
        assert!(policy.check_tool("read_file").is_ok());
        assert!(policy.check_tool("write_file").is_err());
    }

    #[test]
    fn executor_check_tool_delegates_to_policy() {
        let policy = ToolPolicy::new(vec![], vec![]).with_permitted_tools(vec!["read_file".into()]);
        let executor = ToolExecutor::new(policy, PathBuf::from("/tmp"), Duration::from_secs(10));
        assert!(executor.check_tool("read_file").is_ok());
        assert!(executor.check_tool("write_file").is_err());
        assert!(executor.check_tool("edit_file").is_err());
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
    // Tool trait tests
    // -----------------------------------------------------------------------

    /// Minimal Tool implementation for testing the trait contract.
    struct EchoTool {
        def: ToolDefinition,
    }

    impl Tool for EchoTool {
        fn definition(&self) -> &ToolDefinition {
            &self.def
        }

        async fn call(&self, call: ToolCall) -> Result<ToolCallResult> {
            Ok(ToolCallResult {
                tool_call_id: call.id,
                content: format!("echo: {}", call.input),
                is_error: false,
            })
        }
    }

    fn echo_tool() -> EchoTool {
        EchoTool {
            def: ToolDefinition {
                name: "echo".into(),
                description: "Echoes the input back".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {"type": "string"}
                    }
                }),
            },
        }
    }

    #[test]
    fn tool_trait_definition_name() {
        let tool = echo_tool();
        assert_eq!(tool.definition().name, "echo");
        assert_eq!(tool.definition().description, "Echoes the input back");
    }

    #[tokio::test]
    async fn tool_trait_call_returns_ok() {
        let tool = echo_tool();
        let call = ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            input: serde_json::json!("hello world"),
        };
        let result = tool.call(call).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.tool_call_id, "c1");
        assert!(result.content.contains("echo:"));
    }

    #[tokio::test]
    async fn tool_trait_call_id_propagated() {
        let tool = echo_tool();
        let call = ToolCall {
            id: "unique-id-42".into(),
            name: "echo".into(),
            input: serde_json::Value::Null,
        };
        let result = tool.call(call).await.unwrap();
        assert_eq!(result.tool_call_id, "unique-id-42");
    }
}
