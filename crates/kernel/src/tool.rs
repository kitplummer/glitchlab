use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

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
    pub fn check(&self, command: &str) -> std::result::Result<(), String> {
        let trimmed = command.trim();

        // Blocklist takes priority.
        for pattern in &self.blocked {
            if trimmed.contains(pattern.as_str()) {
                return Err(format!(
                    "blocked by pattern `{pattern}`"
                ));
            }
        }

        // Must match at least one allowlist prefix.
        for prefix in &self.allowed {
            if trimmed.starts_with(prefix.as_str()) {
                return Ok(());
            }
        }

        Err("no matching allowlist entry".into())
    }
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
        self.policy.check(command).map_err(|reason| Error::ToolViolation {
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
        let policy = ToolPolicy::new(
            vec!["curl".into()],
            vec!["curl".into()],
        );
        assert!(policy.check("curl http://example.com").is_err());
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
}
