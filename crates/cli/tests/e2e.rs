//! End-to-end functional tests for the GLITCHLAB CLI.
//!
//! These tests invoke the `glitchlab` binary as a subprocess against a mock
//! Anthropic HTTP server, exercising the full chain:
//!   CLI binary → config loading → router → provider (mock) → pipeline →
//!   agents → workspace → history recording → history.jsonl on disk
//!
//! No real LLM API keys are needed.

use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

// ---------------------------------------------------------------------------
// Mock Anthropic API server
// ---------------------------------------------------------------------------

/// Metadata-only agent JSON response — valid for all 6 agent roles.
/// No file content or patches; tools handle file writes.
const AGENT_RESPONSE: &str = r#"{"result": "ok", "steps": [{"step_number": 1, "description": "add feature", "files": ["src/new.rs"], "action": "create"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "trivial", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false, "verdict": "pass", "issues": [], "version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": [], "diagnosis": "none", "root_cause": "none", "files_changed": ["src/new.rs"], "confidence": "high", "should_retry": false, "notes": null, "tests_added": [], "commit_message": "feat: add greet function", "summary": "test", "adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#;

/// Start a mock Anthropic API server that handles multiple requests.
/// Returns the join handle and port.
///
/// The server is request-aware to support tool-use flow:
/// - If the request has `"tools"` and no `"tool_result"` in messages → tool_use response
/// - Otherwise → end_turn text response with `AGENT_RESPONSE`
async fn start_mock_anthropic() -> (tokio::task::JoinHandle<()>, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                // Read the full HTTP request including body.
                let body = read_request(&mut stream).await;

                // Decide which response to send based on request content.
                let api_body = if is_first_tool_call(&body) {
                    // First implementer/debugger call: return a write_file tool use.
                    serde_json::json!({
                        "content": [{"type": "tool_use", "id": "toolu_01", "name": "write_file", "input": {"path": "src/new.rs", "content": "pub fn greet() -> &'static str { \"hello\" }\n"}}],
                        "usage": {"input_tokens": 100, "output_tokens": 50},
                        "stop_reason": "tool_use"
                    }).to_string()
                } else {
                    // All other calls: return end_turn with agent metadata.
                    serde_json::json!({
                        "content": [{"type": "text", "text": AGENT_RESPONSE}],
                        "usage": {"input_tokens": 100, "output_tokens": 50},
                        "stop_reason": "end_turn"
                    })
                    .to_string()
                };

                let resp = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\
                     \r\n\
                     {}",
                    api_body.len(),
                    api_body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    (handle, port)
}

/// Check if this is the first tool-use call (has tools, no tool_result yet).
fn is_first_tool_call(body: &str) -> bool {
    // If body is empty or not JSON, default to end_turn.
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    // Must have a "tools" array.
    let has_tools =
        json["tools"].is_array() && !json["tools"].as_array().unwrap_or(&vec![]).is_empty();
    if !has_tools {
        return false;
    }
    // Must NOT have any tool_result content block in messages.
    let has_tool_result = json["messages"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .any(|msg| {
            msg["content"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .any(|block| block["type"] == "tool_result")
        });
    !has_tool_result
}

/// Read the full HTTP request from the stream (headers + body), return the body.
async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::with_capacity(65536);
    let mut tmp = [0u8; 16384];

    // Read until we find the header/body separator.
    loop {
        let n = stream.read(&mut tmp).await.unwrap_or(0);
        if n == 0 {
            return String::new();
        }
        buf.extend_from_slice(&tmp[..n]);

        // Check if we've received the full headers.
        if let Some(header_end) = find_header_end(&buf) {
            // Try to parse Content-Length and read remaining body.
            let headers = String::from_utf8_lossy(&buf[..header_end]);
            let content_length = headers
                .lines()
                .find(|l| l.to_lowercase().starts_with("content-length:"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);

            let body_start = header_end + 4; // skip \r\n\r\n
            let body_received = buf.len().saturating_sub(body_start);
            let remaining = content_length.saturating_sub(body_received);

            if remaining > 0 {
                let mut body_buf = vec![0u8; remaining];
                let mut read = 0;
                while read < remaining {
                    let n = stream.read(&mut body_buf[read..]).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    read += n;
                }
                buf.extend_from_slice(&body_buf[..read]);
            }
            return String::from_utf8_lossy(&buf[body_start..]).to_string();
        }
    }
}

/// Find the position of `\r\n\r\n` in a buffer.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn init_test_repo(dir: &Path) -> String {
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::fs::write(dir.join("README.md"), "# Test").unwrap();
    std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(dir)
        .output()
        .unwrap();

    let output = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Get the path to the built `glitchlab` binary.
fn glitchlab_bin() -> String {
    env!("CARGO_BIN_EXE_glitchlab").to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// True E2E: invoke the CLI binary, verify it runs the full pipeline and
/// records history to disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_run_records_history() {
    let (server_handle, port) = start_mock_anthropic().await;
    let dir = tempfile::tempdir().unwrap();
    let _base_branch = init_test_repo(dir.path());

    // Step 1: `glitchlab init`
    let init_output = Command::new(glitchlab_bin())
        .args(["init", dir.path().to_str().unwrap()])
        .output()
        .await
        .unwrap();
    assert!(
        init_output.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&init_output.stderr)
    );

    // Commit the .glitchlab directory so the worktree sees it.
    std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "add glitchlab config"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    // Step 2: `glitchlab run` with mock server
    let run_output = Command::new(glitchlab_bin())
        .args([
            "run",
            "--repo",
            dir.path().to_str().unwrap(),
            "--objective",
            "Add a greet function to src/lib.rs",
            "--auto-approve",
            "--test",
            "true",
        ])
        .env("ANTHROPIC_API_KEY", "test-key-e2e")
        .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"))
        .output()
        .await
        .unwrap();

    let stderr = String::from_utf8_lossy(&run_output.stderr);

    // Pipeline should succeed (Committed — no remote to push to).
    assert!(
        run_output.status.success(),
        "glitchlab run failed (exit {:?}):\n{stderr}",
        run_output.status.code()
    );
    assert!(
        stderr.contains("Pipeline Result"),
        "should print pipeline result to stderr:\n{stderr}"
    );

    // Step 3: verify history.jsonl exists and contains the entry
    let history_file = dir.path().join(".glitchlab/logs/history.jsonl");
    assert!(
        history_file.exists(),
        "history.jsonl should exist after pipeline run"
    );

    let contents = std::fs::read_to_string(&history_file).unwrap();
    assert!(!contents.is_empty(), "history.jsonl should not be empty");

    let entry: serde_json::Value = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
    assert!(
        entry["task_id"]
            .as_str()
            .unwrap_or("")
            .starts_with("inline-"),
        "task_id should start with 'inline-', got: {}",
        entry["task_id"]
    );
    assert!(
        entry["status"] == "committed" || entry["status"] == "pr_created",
        "unexpected status: {}",
        entry["status"]
    );

    // Step 4: `glitchlab history` shows the recorded run
    let hist_output = Command::new(glitchlab_bin())
        .args([
            "history",
            "--repo",
            dir.path().to_str().unwrap(),
            "--count",
            "1",
        ])
        .output()
        .await
        .unwrap();
    let hist_stderr = String::from_utf8_lossy(&hist_output.stderr);
    let hist_stdout = String::from_utf8_lossy(&hist_output.stdout);
    assert!(
        hist_output.status.success(),
        "history command failed: {hist_stderr}"
    );
    assert!(
        hist_stdout.contains("inline") || hist_stderr.contains("inline"),
        "history output should mention task 'inline':\nstdout: {hist_stdout}\nstderr: {hist_stderr}"
    );

    server_handle.abort();
}

/// Test the version subcommand.
#[tokio::test]
async fn cli_version_command() {
    let output = Command::new(glitchlab_bin())
        .args(["version"])
        .output()
        .await
        .unwrap();

    assert!(
        output.status.success(),
        "version command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim() == "glitchlab 0.1.0",
        "expected 'glitchlab 0.1.0', got: '{}'",
        stdout.trim()
    );
}

/// E2E failure path: tests fail → history records the failure → `glitchlab history`
/// shows it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_failure_records_history() {
    let (server_handle, port) = start_mock_anthropic().await;
    let dir = tempfile::tempdir().unwrap();
    let _base_branch = init_test_repo(dir.path());

    // Init glitchlab.
    let _ = Command::new(glitchlab_bin())
        .args(["init", dir.path().to_str().unwrap()])
        .output()
        .await
        .unwrap();

    // Add a Makefile with a failing test target.
    std::fs::write(
        dir.path().join("Makefile"),
        "test:\n\t@echo FAIL && exit 1\n",
    )
    .unwrap();

    std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "add glitchlab config and failing test"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    // Run pipeline — should fail because tests fail.
    let run_output = Command::new(glitchlab_bin())
        .args([
            "run",
            "--repo",
            dir.path().to_str().unwrap(),
            "--objective",
            "Fix the broken module",
            "--auto-approve",
        ])
        .env("ANTHROPIC_API_KEY", "test-key-e2e")
        .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"))
        .output()
        .await
        .unwrap();

    // Pipeline should fail (non-zero exit).
    assert!(
        !run_output.status.success(),
        "expected failure, but got success"
    );

    // History should still be recorded.
    let history_file = dir.path().join(".glitchlab/logs/history.jsonl");
    assert!(
        history_file.exists(),
        "history.jsonl should exist even after failure"
    );

    let contents = std::fs::read_to_string(&history_file).unwrap();
    let entry: serde_json::Value = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
    assert!(
        entry["task_id"]
            .as_str()
            .unwrap_or("")
            .starts_with("inline-"),
        "task_id should start with 'inline-', got: {}",
        entry["task_id"]
    );
    assert!(
        entry["status"] != "committed" && entry["status"] != "pr_created",
        "status should indicate failure, got: {}",
        entry["status"]
    );

    // `glitchlab history` should show the failure.
    let hist_output = Command::new(glitchlab_bin())
        .args(["history", "--repo", dir.path().to_str().unwrap(), "--stats"])
        .output()
        .await
        .unwrap();
    let hist_stdout = String::from_utf8_lossy(&hist_output.stdout);
    let hist_stderr = String::from_utf8_lossy(&hist_output.stderr);
    let combined = format!("{hist_stdout}{hist_stderr}").to_lowercase();
    assert!(
        combined.contains("failures"),
        "stats should mention failures:\nstdout: {hist_stdout}\nstderr: {hist_stderr}"
    );

    server_handle.abort();
}
