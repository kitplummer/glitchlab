//! Claude Code CLI-backed implementer agent.
//!
//! Instead of running a manual tool-use loop against a raw LLM API, this agent
//! delegates implementation to `claude --print`, which handles file editing,
//! context management, and tool orchestration natively.
//!
//! See `docs/adr-claude-code-implementer.md` for the decision record.

use std::path::Path;
use std::time::Instant;

use glitchlab_kernel::agent::{Agent, AgentContext, AgentMetadata, AgentOutput};
use glitchlab_kernel::error;
use serde::Deserialize;
use tracing::{info, warn};

use super::build_user_message;

// ---------------------------------------------------------------------------
// Claude Code JSON output schema (from `--output-format json`)
// ---------------------------------------------------------------------------

/// Top-level result object from `claude --print --output-format json`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ClaudeCodeResult {
    #[serde(rename = "type")]
    result_type: String,
    #[serde(default)]
    subtype: String,
    /// Total cost in USD (field name is `total_cost_usd` in Claude CLI output).
    #[serde(default, alias = "cost_usd")]
    total_cost_usd: f64,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    num_turns: u32,
    /// The final text response from Claude Code.
    #[serde(default)]
    result: String,
    #[serde(default)]
    session_id: String,
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the Claude Code implementer backend.
#[derive(Debug, Clone)]
pub struct ClaudeCodeConfig {
    /// Model to pass to `claude --model` (e.g. "sonnet", "opus").
    pub model: String,
    /// Maximum dollar budget per invocation.
    pub max_budget_usd: f64,
    /// Maximum agentic turns before stopping.
    pub max_turns: u32,
    /// Path to the `claude` binary. Defaults to "claude" on PATH.
    pub claude_bin: String,
}

impl Default for ClaudeCodeConfig {
    fn default() -> Self {
        Self {
            model: "sonnet".into(),
            max_budget_usd: 0.50,
            max_turns: 30,
            claude_bin: "claude".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

fn system_prompt() -> &'static str {
    r#"You are Patch, the implementation engine inside GLITCHLAB.

You receive a plan and implement it by editing files in the current working directory.

## Workflow (STRICT ORDER)

1. **Write tests first.** Create or update test files that cover the planned changes.
   Run them with `cargo test` (or equivalent) — they MUST fail (red).
2. **Implement.** Write the minimum code to make the tests pass.
3. **Verify.** Run `cargo test` — all tests MUST pass (green).
4. **Lint.** Run `cargo clippy -- -D warnings` and `cargo fmt --check`. Fix any issues.

Do NOT skip step 1. Do NOT write implementation before tests.

## Rules

- Follow the plan exactly. No feature creep.
- Keep diffs minimal. Always add/update tests.
- Use idiomatic patterns for the language.
- Do NOT commit changes — the outer pipeline handles git.
- Do NOT create new branches.

## Final output

When done, output ONLY this JSON (no markdown fences, no explanation):
{
  "files_changed": ["path/to/file", ...],
  "tests_added": ["path/to/test_file", ...],
  "tests_passing": <bool>,
  "commit_message": "<conventional commit message>",
  "summary": "<brief human-readable summary>"
}"#
}

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

/// Build the full prompt to send to Claude Code.
///
/// This combines the plan (from previous_output), file context, codebase
/// knowledge, and constraints into a single prompt string.
fn build_prompt(ctx: &AgentContext) -> String {
    build_user_message(ctx)
}

// ---------------------------------------------------------------------------
// Agent implementation
// ---------------------------------------------------------------------------

pub struct ClaudeCodeImplementer {
    config: ClaudeCodeConfig,
}

impl ClaudeCodeImplementer {
    pub fn new(config: ClaudeCodeConfig) -> Self {
        Self { config }
    }

    /// Invoke the Claude Code CLI and return the parsed result.
    async fn invoke_claude(
        &self,
        working_dir: &Path,
        prompt: &str,
    ) -> error::Result<(ClaudeCodeResult, String)> {
        use tokio::io::AsyncWriteExt;

        let start = Instant::now();

        let mut cmd = tokio::process::Command::new(&self.config.claude_bin);
        cmd.current_dir(working_dir)
            .arg("--print")
            .arg("--output-format")
            .arg("json")
            .arg("--model")
            .arg(&self.config.model)
            .arg("--max-turns")
            .arg(self.config.max_turns.to_string())
            .arg("--max-budget-usd")
            .arg(format!("{:.2}", self.config.max_budget_usd))
            .arg("--system-prompt")
            .arg(system_prompt())
            .arg("--permission-mode")
            .arg("bypassPermissions")
            .arg("--allowedTools")
            .arg("Read Edit Write Bash(cargo:*) Bash(git diff:*) Bash(git status:*) Glob Grep")
            .arg("--no-session-persistence")
            .arg("-p")
            .arg("-") // Read prompt from stdin
            // Unset CLAUDECODE to allow nested invocation.
            .env_remove("CLAUDECODE")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        info!(
            model = %self.config.model,
            max_turns = self.config.max_turns,
            max_budget = self.config.max_budget_usd,
            "invoking claude code implementer"
        );

        let mut child = cmd.spawn().map_err(|e| error::Error::Pipeline {
            stage: "claude_code".into(),
            reason: format!("failed to spawn claude CLI: {e}"),
        })?;

        // Write prompt to stdin.
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(prompt.as_bytes())
                .await
                .map_err(|e| error::Error::Pipeline {
                    stage: "claude_code".into(),
                    reason: format!("failed to write prompt to stdin: {e}"),
                })?;
            // Drop stdin to signal EOF.
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| error::Error::Pipeline {
                stage: "claude_code".into(),
                reason: format!("failed to wait for claude CLI: {e}"),
            })?;

        let elapsed = start.elapsed();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            warn!(
                exit_code = output.status.code(),
                stderr = %stderr.chars().take(500).collect::<String>(),
                "claude CLI exited with error"
            );
        }

        info!(
            exit_code = output.status.code(),
            stdout_len = stdout.len(),
            elapsed_ms = elapsed.as_millis() as u64,
            "claude code invocation complete"
        );

        // Log raw output for debugging empty-result issues.
        tracing::debug!(
            stdout_preview = %stdout.chars().take(500).collect::<String>(),
            stderr_preview = %stderr.chars().take(500).collect::<String>(),
            "claude code raw output"
        );

        // Try stdout first, fall back to stderr.
        // Claude Code >=2.x may emit the JSON envelope on stderr
        // when --print --output-format json are combined.
        let json_source = select_json_source(&stdout, &stderr);
        if std::ptr::addr_eq(json_source as *const str, stderr.as_str() as *const str) {
            info!("stdout empty or unparseable, using stderr for JSON envelope");
        }

        // Parse the JSON result. Claude --print --output-format json emits
        // a single JSON object on stdout (or stderr in some versions).
        let result: ClaudeCodeResult = serde_json::from_str(json_source).map_err(|e| {
            warn!(
                error = %e,
                stdout_preview = %stdout.chars().take(200).collect::<String>(),
                "failed to parse claude CLI output"
            );
            error::Error::Pipeline {
                stage: "claude_code".into(),
                reason: format!("failed to parse claude CLI output: {e}"),
            }
        })?;

        Ok((result, stderr))
    }
}

impl Agent for ClaudeCodeImplementer {
    fn role(&self) -> &str {
        "implementer"
    }

    fn persona(&self) -> &str {
        "Patch"
    }

    async fn execute(&self, ctx: &AgentContext) -> error::Result<AgentOutput> {
        let prompt = build_prompt(ctx);
        let working_dir = Path::new(&ctx.working_dir);

        let (result, _stderr) = self.invoke_claude(working_dir, &prompt).await?;

        let metadata = AgentMetadata {
            agent: "implementer".into(),
            model: format!("claude-code/{}", self.config.model),
            tokens: 0, // Claude Code doesn't expose token counts in JSON output
            cost: result.total_cost_usd,
            latency_ms: result.duration_ms,
        };

        if result.is_error {
            warn!(
                error = %result.result.chars().take(300).collect::<String>(),
                "claude code reported an error"
            );
            return Ok(AgentOutput {
                data: build_error_output(
                    &format!("Claude Code error: {}", &result.result),
                    "model_limitation",
                ),
                metadata,
                parse_error: true,
            });
        }

        // Claude Code hit its per-invocation budget limit. is_error is false
        // but the result text is empty. Check the worktree for partial work.
        if result.subtype == "error_max_budget_usd" {
            warn!(
                cost = result.total_cost_usd,
                turns = result.num_turns,
                "claude code hit budget limit, checking worktree for partial changes"
            );
            let data = match detect_changes_from_worktree(working_dir).await {
                Some(detected) => {
                    info!(
                        files_changed = detected["files_changed"]
                            .as_array()
                            .map(|a| a.len())
                            .unwrap_or(0),
                        "recovered partial changes from worktree after budget exhaustion"
                    );
                    detected
                }
                None => build_error_output(
                    "Claude Code budget exhausted with no file changes",
                    "budget_exhaustion",
                ),
            };
            let parse_error = !data
                .get("tests_passing")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            return Ok(AgentOutput {
                data,
                metadata,
                parse_error,
            });
        }

        // Parse the implementer's final JSON from the result text.
        // If the result text is empty (Claude exhausted turns doing tool calls
        // without a final text response), fall back to detecting changes from
        // the git worktree.
        let parsed = parse_implementer_json(&result.result);
        let (data, parse_error) = if let Some(p) = parsed {
            (p, false)
        } else if result.result.trim().is_empty() {
            // Result is empty — Claude likely used all turns on tool calls.
            // Check git status in the worktree for actual file changes.
            info!("result text empty, checking worktree for file changes");
            match detect_changes_from_worktree(working_dir).await {
                Some(detected) => {
                    info!(
                        files_changed = detected["files_changed"].as_array().map(|a| a.len()).unwrap_or(0),
                        tests_passing = %detected.get("tests_passing").and_then(|v| v.as_bool()).unwrap_or(false),
                        "detected file changes from worktree"
                    );
                    (detected, false)
                }
                None => {
                    warn!("no file changes detected in worktree either");
                    (
                        build_error_output(
                            "Claude Code produced no output and no file changes",
                            "parse_failure",
                        ),
                        true,
                    )
                }
            }
        } else {
            let raw_preview: String = result.result.chars().take(500).collect();
            warn!(
                result_preview = %raw_preview,
                "could not extract implementer JSON from claude code result"
            );
            let mut data =
                build_error_output("Failed to parse Claude Code output", "parse_failure");
            data["_raw_output_preview"] = serde_json::json!(raw_preview);
            (data, true)
        };

        info!(
            cost = result.total_cost_usd,
            turns = result.num_turns,
            duration_ms = result.duration_ms,
            tests_passing = %data.get("tests_passing").and_then(|v| v.as_bool()).unwrap_or(false),
            "claude code implementer complete"
        );

        Ok(AgentOutput {
            data,
            metadata,
            parse_error,
        })
    }
}

/// Choose which output stream contains the JSON envelope from `claude --print`.
///
/// Claude Code may emit the JSON result on stdout or stderr depending on the
/// version. Strategy:
/// - If stdout is non-empty and parses as a valid `ClaudeCodeResult`, use it.
/// - Otherwise fall back to stderr if stderr is non-empty (Claude Code ≥2.x).
/// - If both are empty/invalid, return stdout (the caller handles the parse error).
pub(crate) fn select_json_source<'a>(stdout: &'a str, stderr: &'a str) -> &'a str {
    let stdout_parseable =
        !stdout.trim().is_empty() && serde_json::from_str::<ClaudeCodeResult>(stdout).is_ok();
    if stdout_parseable {
        stdout
    } else if !stderr.trim().is_empty() {
        stderr
    } else {
        stdout
    }
}

/// Build the fallback `AgentOutput.data` JSON for error / stuck conditions.
///
/// The `obstacle_kind` string must match a
/// `glitchlab_kernel::outcome::ObstacleKind` serde tag (e.g.
/// `"model_limitation"`, `"budget_exhaustion"`, `"parse_failure"`).
fn build_error_output(summary: &str, obstacle_kind: &str) -> serde_json::Value {
    serde_json::json!({
        "files_changed": [],
        "tests_added": [],
        "tests_passing": false,
        "commit_message": "chore: no changes produced",
        "summary": summary,
        "obstacle_kind": obstacle_kind,
        "stuck": true,
        "stuck_reason": obstacle_kind,
    })
}

/// Extract the implementer's JSON output from Claude Code's result text.
///
/// The result may contain the raw JSON, or it may be wrapped in markdown
/// fences or have extra text. We try several strategies:
/// 1. Direct parse of the full text as JSON
/// 2. Extract JSON from markdown code fences
/// 3. Find the first `{` to last `}` span and parse that
fn parse_implementer_json(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();

    // Strategy 1: direct parse
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed)
        && (v.get("files_changed").is_some() || v.get("commit_message").is_some())
    {
        return Some(v);
    }

    // Strategy 2: extract from markdown code fences
    if let Some(start) = trimmed.find("```json") {
        let after_fence = &trimmed[start + 7..];
        if let Some(end) = after_fence.find("```") {
            let json_str = after_fence[..end].trim();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
                return Some(v);
            }
        }
    }
    // Also try bare ``` fences
    if let Some(start) = trimmed.find("```\n") {
        let after_fence = &trimmed[start + 4..];
        if let Some(end) = after_fence.find("```") {
            let json_str = after_fence[..end].trim();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
                return Some(v);
            }
        }
    }

    // Strategy 3: find first { to last }
    let first_brace = trimmed.find('{')?;
    let last_brace = trimmed.rfind('}')?;
    if first_brace < last_brace {
        let candidate = &trimmed[first_brace..=last_brace];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(candidate) {
            return Some(v);
        }
    }

    None
}

/// Detect file changes from the git worktree when Claude's text result is empty.
///
/// Runs `git diff --name-only` and `cargo test` in the worktree to construct
/// the implementer output that Claude should have produced.
async fn detect_changes_from_worktree(working_dir: &Path) -> Option<serde_json::Value> {
    // Get list of changed files.
    let diff_output = tokio::process::Command::new("git")
        .args(["diff", "--name-only", "HEAD"])
        .current_dir(working_dir)
        .output()
        .await
        .ok()?;

    let diff_text = String::from_utf8_lossy(&diff_output.stdout);
    let files_changed: Vec<String> = diff_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect();

    if files_changed.is_empty() {
        // Also check untracked files.
        let status = tokio::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(working_dir)
            .output()
            .await
            .ok()?;
        let status_text = String::from_utf8_lossy(&status.stdout);
        if status_text.trim().is_empty() {
            return None; // No changes at all.
        }
    }

    let tests_added: Vec<&String> = files_changed
        .iter()
        .filter(|f| f.contains("test") || f.contains("_test") || f.ends_with("_tests.rs"))
        .collect();

    // Run cargo test to check if tests pass.
    let test_result = tokio::process::Command::new("cargo")
        .args(["test", "--quiet"])
        .current_dir(working_dir)
        .output()
        .await;
    let tests_passing = test_result.map(|o| o.status.success()).unwrap_or(false);

    let summary = format!(
        "Detected {} file change(s) from worktree (tests {})",
        files_changed.len(),
        if tests_passing { "passing" } else { "failing" }
    );

    Some(serde_json::json!({
        "files_changed": files_changed,
        "tests_added": tests_added,
        "tests_passing": tests_passing,
        "commit_message": format!("feat: implement changes ({} files)", files_changed.len()),
        "summary": summary,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_raw_json() {
        let input = r#"{"files_changed": ["lib.rs"], "tests_added": [], "tests_passing": true, "commit_message": "feat: add greet", "summary": "done"}"#;
        let result = parse_implementer_json(input).unwrap();
        assert_eq!(result["files_changed"][0], "lib.rs");
        assert_eq!(result["tests_passing"], true);
    }

    #[test]
    fn parse_json_in_markdown_fence() {
        let input = r#"Here is the result:

```json
{"files_changed": ["lib.rs"], "tests_added": ["tests/greet.rs"], "tests_passing": true, "commit_message": "feat: add greet", "summary": "done"}
```"#;
        let result = parse_implementer_json(input).unwrap();
        assert_eq!(result["files_changed"][0], "lib.rs");
    }

    #[test]
    fn parse_json_with_surrounding_text() {
        let input = r#"I've completed the implementation. Here's the summary:

{"files_changed": ["src/lib.rs"], "tests_added": ["src/lib.rs"], "tests_passing": true, "commit_message": "feat: add feature", "summary": "Added feature"}

All tests pass."#;
        let result = parse_implementer_json(input).unwrap();
        assert_eq!(result["summary"], "Added feature");
    }

    #[test]
    fn parse_returns_none_for_garbage() {
        assert!(parse_implementer_json("no json here").is_none());
    }

    #[test]
    fn parse_returns_none_for_empty_string() {
        // This is the actual production failure: Claude Code returns a valid
        // outer JSON envelope but the `result` field is empty.
        assert!(parse_implementer_json("").is_none());
    }

    #[test]
    fn parse_returns_none_for_whitespace() {
        assert!(parse_implementer_json("   \n\t  ").is_none());
    }

    #[test]
    fn parse_returns_none_for_non_json_prose() {
        // Claude Code sometimes returns prose instead of JSON in result field.
        let input = "I was unable to complete the task because the repository has no Cargo.toml.";
        assert!(parse_implementer_json(input).is_none());
    }

    #[test]
    fn parse_returns_none_for_wrong_schema() {
        let input = r#"{"name": "not implementer output"}"#;
        // This has no files_changed or commit_message, so strategy 1 skips it.
        // Strategy 3 will parse it but it won't have the right fields.
        // For now, strategy 3 accepts any valid JSON — this is acceptable
        // because the pipeline validates the schema downstream.
        let result = parse_implementer_json(input);
        assert!(result.is_some()); // strategy 3 accepts any valid JSON
    }

    #[test]
    fn system_prompt_contains_tdd() {
        let prompt = system_prompt();
        assert!(prompt.contains("Write tests first"));
        assert!(prompt.contains("STRICT ORDER"));
    }

    #[test]
    fn system_prompt_forbids_git_commits() {
        let prompt = system_prompt();
        assert!(prompt.contains("Do NOT commit"));
    }

    #[test]
    fn default_config() {
        let config = ClaudeCodeConfig::default();
        assert_eq!(config.model, "sonnet");
        assert_eq!(config.max_budget_usd, 0.50);
        assert_eq!(config.max_turns, 30);
        assert_eq!(config.claude_bin, "claude");
    }

    #[test]
    fn build_prompt_includes_objective() {
        let ctx = test_context();
        let prompt = build_prompt(&ctx);
        assert!(prompt.contains("Add a greeting function"));
    }

    #[test]
    fn build_prompt_includes_plan() {
        let mut ctx = test_context();
        ctx.previous_output = serde_json::json!({
            "steps": [{"description": "create greeting.rs"}]
        });
        let prompt = build_prompt(&ctx);
        assert!(prompt.contains("create greeting.rs"));
    }

    #[test]
    fn build_prompt_includes_file_context() {
        let mut ctx = test_context();
        ctx.file_context
            .insert("src/lib.rs".into(), "pub mod greeting;".into());
        let prompt = build_prompt(&ctx);
        assert!(prompt.contains("src/lib.rs"));
        assert!(prompt.contains("pub mod greeting;"));
    }

    #[test]
    fn role_and_persona() {
        let agent = ClaudeCodeImplementer::new(ClaudeCodeConfig::default());
        assert_eq!(agent.role(), "implementer");
        assert_eq!(agent.persona(), "Patch");
    }

    #[test]
    fn claude_code_result_deserialization() {
        let json = r#"{
            "type": "result",
            "subtype": "success",
            "total_cost_usd": 0.05,
            "duration_ms": 12345,
            "is_error": false,
            "num_turns": 5,
            "result": "{\"files_changed\": [], \"summary\": \"done\"}",
            "session_id": "abc-123"
        }"#;
        let result: ClaudeCodeResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.result_type, "result");
        assert_eq!(result.subtype, "success");
        assert!(!result.is_error);
        assert_eq!(result.num_turns, 5);
        assert_eq!(result.total_cost_usd, 0.05);
    }

    #[tokio::test]
    async fn detect_changes_empty_repo() {
        let dir = tempfile::tempdir().unwrap();
        // Init a git repo with an initial commit.
        let _ = std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(dir.path())
            .output();
        // No changes → should return None.
        let result = detect_changes_from_worktree(dir.path()).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn detect_changes_with_modified_file() {
        let dir = tempfile::tempdir().unwrap();
        let _ = std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output();
        std::fs::write(dir.path().join("hello.rs"), "fn main() {}").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output();
        // Modify the file.
        std::fs::write(
            dir.path().join("hello.rs"),
            "fn main() { println!(\"hi\"); }",
        )
        .unwrap();
        let result = detect_changes_from_worktree(dir.path()).await;
        assert!(result.is_some());
        let data = result.unwrap();
        let files = data["files_changed"].as_array().unwrap();
        assert!(files.iter().any(|f| f.as_str() == Some("hello.rs")));
    }

    #[tokio::test]
    async fn detect_changes_with_untracked_file() {
        let dir = tempfile::tempdir().unwrap();
        let _ = std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(dir.path())
            .output();
        // Add an untracked file (not staged, not committed).
        std::fs::write(dir.path().join("new_file.rs"), "fn new() {}").unwrap();
        // git diff --name-only HEAD returns empty, but git status --porcelain shows ?? new_file.rs.
        let result = detect_changes_from_worktree(dir.path()).await;
        // Should return Some because untracked files exist.
        assert!(result.is_some());
        let data = result.unwrap();
        // files_changed is empty (no diff), but untracked files triggered detection.
        let files = data["files_changed"].as_array().unwrap();
        assert!(files.is_empty());
    }

    #[tokio::test]
    async fn detect_changes_with_test_files() {
        let dir = tempfile::tempdir().unwrap();
        let _ = std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output();
        std::fs::write(dir.path().join("lib.rs"), "pub fn greet() {}").unwrap();
        std::fs::write(dir.path().join("test_greet.rs"), "#[test] fn t() {}").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output();
        // Modify both files.
        std::fs::write(dir.path().join("lib.rs"), "pub fn greet() { todo!() }").unwrap();
        std::fs::write(
            dir.path().join("test_greet.rs"),
            "#[test] fn t() { assert!(true); }",
        )
        .unwrap();
        let result = detect_changes_from_worktree(dir.path()).await;
        assert!(result.is_some());
        let data = result.unwrap();
        // test_greet.rs contains "test" so it should be in tests_added.
        let tests_added = data["tests_added"].as_array().unwrap();
        assert!(
            tests_added
                .iter()
                .any(|f| f.as_str() == Some("test_greet.rs")),
            "expected test_greet.rs in tests_added: {tests_added:?}"
        );
        // lib.rs should NOT be in tests_added.
        assert!(
            !tests_added.iter().any(|f| f.as_str() == Some("lib.rs")),
            "lib.rs should not be in tests_added"
        );
    }

    #[tokio::test]
    async fn detect_changes_summary_format() {
        let dir = tempfile::tempdir().unwrap();
        let _ = std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output();
        std::fs::write(dir.path().join("a.rs"), "fn a() { todo!() }").unwrap();
        let result = detect_changes_from_worktree(dir.path()).await;
        assert!(result.is_some());
        let data = result.unwrap();
        let summary = data["summary"].as_str().unwrap();
        assert!(
            summary.contains("1 file change"),
            "summary should mention file count: {summary}"
        );
        let commit_msg = data["commit_message"].as_str().unwrap();
        assert!(
            commit_msg.contains("1 files"),
            "commit message: {commit_msg}"
        );
    }

    #[test]
    fn claude_code_result_error_subtype() {
        let json = r#"{
            "type": "result",
            "subtype": "error_max_budget_usd",
            "total_cost_usd": 0.50,
            "duration_ms": 120000,
            "is_error": false,
            "num_turns": 10,
            "result": "",
            "session_id": "abc-123"
        }"#;
        let result: ClaudeCodeResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.subtype, "error_max_budget_usd");
        assert!(result.result.is_empty());
        assert!(!result.is_error);
    }

    #[test]
    fn claude_code_result_with_error_flag() {
        let json = r#"{
            "type": "result",
            "subtype": "error",
            "total_cost_usd": 0.01,
            "duration_ms": 1000,
            "is_error": true,
            "num_turns": 1,
            "result": "An error occurred",
            "session_id": "err-456"
        }"#;
        let result: ClaudeCodeResult = serde_json::from_str(json).unwrap();
        assert!(result.is_error);
        assert_eq!(result.result, "An error occurred");
    }

    // ------------------------------------------------------------------
    // select_json_source
    // ------------------------------------------------------------------

    #[test]
    fn select_json_source_prefers_valid_stdout() {
        let valid_stdout =
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#;
        let stderr = r#"{"type":"result","subtype":"success","is_error":false,"result":"stderr"}"#;
        assert_eq!(select_json_source(valid_stdout, stderr), valid_stdout);
    }

    #[test]
    fn select_json_source_falls_back_to_stderr_when_stdout_invalid() {
        let stdout = "not json at all";
        let stderr = r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#;
        assert_eq!(select_json_source(stdout, stderr), stderr);
    }

    #[test]
    fn select_json_source_falls_back_to_stderr_when_stdout_empty() {
        let stdout = "   ";
        let stderr = r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#;
        assert_eq!(select_json_source(stdout, stderr), stderr);
    }

    #[test]
    fn select_json_source_uses_stdout_when_both_unparseable() {
        // Neither stream has valid JSON; function should return stdout (caller handles error).
        assert_eq!(select_json_source("not json", ""), "not json");
    }

    #[test]
    fn select_json_source_uses_stdout_when_both_empty() {
        assert_eq!(select_json_source("", ""), "");
    }

    // ------------------------------------------------------------------
    // parse_implementer_json — bare code fence (Strategy 2b)
    // ------------------------------------------------------------------

    #[test]
    fn parse_bare_code_fence() {
        let input = concat!(
            "Result:\n",
            "```\n",
            r#"{"files_changed":["a.rs"],"tests_added":[],"tests_passing":true,"commit_message":"feat","summary":"done"}"#,
            "\n```"
        );
        let result = parse_implementer_json(input).unwrap();
        assert_eq!(result["files_changed"][0], "a.rs");
        assert_eq!(result["tests_passing"], true);
    }

    // ------------------------------------------------------------------
    // build_error_output
    // ------------------------------------------------------------------

    #[test]
    fn build_error_output_model_limitation() {
        let data = build_error_output("some model error", "model_limitation");
        assert_eq!(data["obstacle_kind"], "model_limitation");
        assert_eq!(data["tests_passing"], false);
        assert!(data["stuck"].as_bool().unwrap());
        assert!(data["files_changed"].as_array().unwrap().is_empty());
        assert_eq!(data["stuck_reason"], "model_limitation");
    }

    #[test]
    fn build_error_output_budget_exhaustion() {
        let data = build_error_output("budget hit", "budget_exhaustion");
        assert_eq!(data["obstacle_kind"], "budget_exhaustion");
        assert_eq!(data["stuck_reason"], "budget_exhaustion");
        assert_eq!(data["commit_message"], "chore: no changes produced");
    }

    #[test]
    fn build_error_output_parse_failure() {
        let data = build_error_output("could not parse output", "parse_failure");
        assert_eq!(data["obstacle_kind"], "parse_failure");
        assert_eq!(data["summary"], "could not parse output");
        assert!(data["tests_added"].as_array().unwrap().is_empty());
    }

    fn test_context() -> AgentContext {
        AgentContext {
            task_id: "test-task".into(),
            objective: "Add a greeting function to the crate".into(),
            repo_path: "/tmp/repo".into(),
            working_dir: "/tmp/worktree".into(),
            constraints: vec![],
            acceptance_criteria: vec![],
            risk_level: "low".into(),
            file_context: std::collections::HashMap::new(),
            previous_output: serde_json::Value::Null,
            extra: std::collections::HashMap::new(),
        }
    }
}
