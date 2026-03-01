//! Beads (task graph) backend via the `bd` CLI subprocess.
//!
//! Supports both write (history recording) and read (backlog loading via
//! `bd list --json`). Reads are parsed into [`Bead`] structs which the
//! eng-org `TaskQueue` can convert to `Task` items.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::error::{MemoryError, Result};
use crate::history::{HistoryBackend, HistoryEntry, HistoryQuery, HistoryStats};

// ---------------------------------------------------------------------------
// Bead data model (mirrors `bd list --json` output)
// ---------------------------------------------------------------------------

/// A single bead/issue from `bd list --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bead {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_bead_status")]
    pub status: String,
    #[serde(default = "default_bead_priority")]
    pub priority: i32,
    #[serde(default)]
    pub issue_type: String,
    #[serde(default)]
    pub dependencies: Vec<BeadDependency>,
    #[serde(default)]
    pub external_ref: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignee: Option<String>,
}

/// A dependency edge between two beads.
///
/// The `bd` CLI outputs `depends_on_id` and `type`; our internal model uses
/// `target_id` and `dep_type`. Serde aliases bridge both conventions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeadDependency {
    #[serde(alias = "depends_on_id")]
    pub target_id: String,
    #[serde(default, alias = "type")]
    pub dep_type: String,
}

/// Request to create a bead with full metadata.
///
/// Used by the ADR decomposer to create beads with title, description,
/// type, priority, and labels in a single `bd create` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeadCreateRequest {
    pub id: String,
    pub title: String,
    pub description: String,
    pub issue_type: String,
    pub priority: i32,
    pub labels: Vec<String>,
}

fn default_bead_status() -> String {
    "open".into()
}

fn default_bead_priority() -> i32 {
    2
}

/// Beads task-graph client wrapping the `bd` CLI.
pub struct BeadsClient {
    repo_path: PathBuf,
    bd_path: String,
}

impl BeadsClient {
    /// Create a new BeadsClient.
    ///
    /// `bd_path` defaults to the `BEADS_BD_PATH` env var, falling back to `"bd"` on PATH.
    pub fn new(repo_path: &Path, bd_path: Option<String>) -> Self {
        let bd = bd_path
            .or_else(|| std::env::var("BEADS_BD_PATH").ok())
            .unwrap_or_else(|| "bd".into());
        Self {
            repo_path: repo_path.to_path_buf(),
            bd_path: bd,
        }
    }

    /// Run a `bd` subcommand and return stdout.
    async fn run_bd(&self, args: &[&str]) -> Result<String> {
        let output = tokio::process::Command::new(&self.bd_path)
            .args(args)
            .current_dir(&self.repo_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| MemoryError::Beads(format!("failed to run bd: {e}")))?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(MemoryError::Beads(format!("bd failed: {stderr}")))
        }
    }

    /// Check if the `bd` binary is installed and reachable.
    pub async fn check_installed(&self) -> bool {
        self.run_bd(&["--version"]).await.is_ok()
    }

    /// Create a new bead for a task.
    pub async fn create_bead(&self, task_id: &str, status: &str) -> Result<String> {
        let title = format!("Task {task_id}: {status}");
        self.run_bd(&[
            "create", &title, "--id", task_id, "--type", "task", "--silent",
        ])
        .await
    }

    /// Link two beads.
    pub async fn link(&self, from: &str, to: &str) -> Result<String> {
        self.run_bd(&["link", from, to]).await
    }

    /// Update a bead's status.
    pub async fn update_status(&self, task_id: &str, status: &str) -> Result<String> {
        self.run_bd(&["update", task_id, "--status", status]).await
    }

    /// Close a bead.
    pub async fn close_bead(&self, task_id: &str) -> Result<String> {
        self.run_bd(&["close", task_id]).await
    }

    /// List beads (raw text output).
    pub async fn list(&self) -> Result<String> {
        self.run_bd(&["list"]).await
    }

    /// List all beads, parsed from JSON.
    pub async fn list_parsed(&self) -> Result<Vec<Bead>> {
        let json = self.run_bd(&["list", "--json"]).await?;
        serde_json::from_str(&json)
            .map_err(|e| MemoryError::Beads(format!("failed to parse bd list: {e}")))
    }

    /// Create a bead with full metadata (title, description, type, priority, labels).
    pub async fn create_bead_detailed(&self, req: &BeadCreateRequest) -> Result<String> {
        let mut args: Vec<&str> = vec!["create", &req.title, "--id", &req.id];
        if !req.issue_type.is_empty() {
            args.push("--type");
            args.push(&req.issue_type);
        }
        if !req.description.is_empty() {
            args.push("--description");
            args.push(&req.description);
        }
        let priority_str = req.priority.to_string();
        args.push("--priority");
        args.push(&priority_str);
        for label in &req.labels {
            args.push("--label");
            args.push(label);
        }
        args.push("--silent");
        self.run_bd(&args).await
    }

    /// List ready (unblocked) beads, parsed from JSON.
    pub async fn ready_parsed(&self) -> Result<Vec<Bead>> {
        let json = self.run_bd(&["ready", "--json"]).await?;
        serde_json::from_str(&json)
            .map_err(|e| MemoryError::Beads(format!("failed to parse bd ready: {e}")))
    }
}

/// Map a PipelineStatus serialization to a bead-native status.
///
/// PipelineStatus is serialized as snake_case strings by serde (e.g., "pr_merged").
/// Beads expects: "open", "in_progress", "closed", "blocked", "deferred".
fn pipeline_status_to_bead_status(pipeline_status: &str) -> &'static str {
    match pipeline_status {
        // Success outcomes → closed
        "pr_created" | "pr_merged" | "committed" | "already_done" => "closed",

        // Decomposition → in_progress (sub-tasks spawned)
        "decomposed" => "in_progress",

        // External dependency needed → blocked
        "blocked" | "escalated" | "security_blocked" => "blocked",

        // Deferrable/transient → deferred
        "deferred" | "retryable" | "timed_out" => "deferred",

        // Failures that need rework → open (available for retry)
        "plan_failed"
        | "implementation_failed"
        | "tests_failed"
        | "parse_error"
        | "boundary_violation"
        | "architect_rejected"
        | "interrupted"
        | "budget_exceeded"
        | "error" => "open",

        // Unknown → open (safe default, task stays visible)
        _ => "open",
    }
}

impl HistoryBackend for BeadsClient {
    fn record<'a>(
        &'a self,
        entry: &'a HistoryEntry,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let bead_status = pipeline_status_to_bead_status(&entry.status);
            // Try updating the existing bead first (task loaded from backlog).
            // If that fails (bead doesn't exist yet), create a new one.
            match self.update_status(&entry.task_id, bead_status).await {
                Ok(_) => Ok(()),
                Err(_) => self
                    .create_bead(&entry.task_id, bead_status)
                    .await
                    .map(|_| ()),
            }
        })
    }

    /// Beads is write-only for history; reads go through Dolt/JSONL.
    fn query<'a>(
        &'a self,
        _query: &'a HistoryQuery,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<HistoryEntry>>> + Send + 'a>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Stats not supported for Beads backend.
    fn stats(&self) -> Pin<Box<dyn Future<Output = Result<HistoryStats>> + Send + '_>> {
        Box::pin(async {
            Err(MemoryError::Unavailable(
                "stats not supported for Beads backend".into(),
            ))
        })
    }

    fn failure_context(
        &self,
        _max_entries: usize,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        Box::pin(async { Ok(String::new()) })
    }

    fn backend_name(&self) -> &str {
        "beads"
    }

    fn is_available(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async { self.check_installed().await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_name_is_beads() {
        let client = BeadsClient::new(Path::new("/tmp"), Some("nonexistent-bd-binary".into()));
        assert_eq!(client.backend_name(), "beads");
    }

    #[tokio::test]
    async fn check_installed_false_when_bd_not_on_path() {
        let client = BeadsClient::new(Path::new("/tmp"), Some("nonexistent-bd-binary-xyz".into()));
        assert!(!client.check_installed().await);
    }

    #[tokio::test]
    async fn run_bd_error_for_nonexistent_binary() {
        let client = BeadsClient::new(Path::new("/tmp"), Some("nonexistent-bd-binary-xyz".into()));
        let result = client.run_bd(&["--version"]).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, MemoryError::Beads(_)));
    }

    #[tokio::test]
    async fn is_available_false_when_bd_missing() {
        let client = BeadsClient::new(Path::new("/tmp"), Some("nonexistent-bd-binary-xyz".into()));
        assert!(!client.is_available().await);
    }

    #[tokio::test]
    async fn query_returns_empty_by_design() {
        let client = BeadsClient::new(Path::new("/tmp"), Some("nonexistent-bd-binary-xyz".into()));
        let query = HistoryQuery::default();
        let results = client.query(&query).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn stats_returns_unavailable() {
        let client = BeadsClient::new(Path::new("/tmp"), Some("nonexistent-bd-binary-xyz".into()));
        let result = client.stats().await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), MemoryError::Unavailable(_)));
    }

    #[tokio::test]
    async fn failure_context_returns_empty() {
        let client = BeadsClient::new(Path::new("/tmp"), Some("nonexistent-bd-binary-xyz".into()));
        let ctx = client.failure_context(5).await.unwrap();
        assert!(ctx.is_empty());
    }

    #[tokio::test]
    async fn mock_subprocess_test() {
        // Use a real command ("echo") standing in for bd to test run_bd plumbing.
        let client = BeadsClient::new(Path::new("/tmp"), Some("echo".into()));
        let result = client.run_bd(&["hello", "world"]).await.unwrap();
        assert!(result.contains("hello"));
        assert!(result.contains("world"));
    }

    // -----------------------------------------------------------------------
    // Bead struct tests
    // -----------------------------------------------------------------------

    #[test]
    fn bead_deserialize_full() {
        let json = r#"{
            "id": "task-42",
            "title": "Fix the widget",
            "description": "The widget is broken",
            "status": "in_progress",
            "priority": 1,
            "issue_type": "bug",
            "dependencies": [{"target_id": "task-1", "dep_type": "blocks"}],
            "external_ref": "https://github.com/org/repo/pull/99",
            "labels": ["urgent"],
            "assignee": "alice"
        }"#;
        let bead: Bead = serde_json::from_str(json).unwrap();
        assert_eq!(bead.id, "task-42");
        assert_eq!(bead.title, "Fix the widget");
        assert_eq!(bead.description, "The widget is broken");
        assert_eq!(bead.status, "in_progress");
        assert_eq!(bead.priority, 1);
        assert_eq!(bead.issue_type, "bug");
        assert_eq!(bead.dependencies.len(), 1);
        assert_eq!(bead.dependencies[0].target_id, "task-1");
        assert_eq!(bead.dependencies[0].dep_type, "blocks");
        assert_eq!(
            bead.external_ref.as_deref(),
            Some("https://github.com/org/repo/pull/99")
        );
        assert_eq!(bead.labels, vec!["urgent"]);
        assert_eq!(bead.assignee.as_deref(), Some("alice"));
    }

    #[test]
    fn bead_deserialize_minimal() {
        let json = r#"{"id": "t1", "title": "Do something"}"#;
        let bead: Bead = serde_json::from_str(json).unwrap();
        assert_eq!(bead.id, "t1");
        assert_eq!(bead.title, "Do something");
        assert_eq!(bead.description, "");
        assert_eq!(bead.status, "open");
        assert_eq!(bead.priority, 2);
        assert_eq!(bead.issue_type, "");
        assert!(bead.dependencies.is_empty());
        assert!(bead.external_ref.is_none());
        assert!(bead.labels.is_empty());
        assert!(bead.assignee.is_none());
    }

    #[test]
    fn bead_deserialize_array() {
        let json = r#"[
            {"id": "a", "title": "First"},
            {"id": "b", "title": "Second", "priority": 0}
        ]"#;
        let beads: Vec<Bead> = serde_json::from_str(json).unwrap();
        assert_eq!(beads.len(), 2);
        assert_eq!(beads[0].id, "a");
        assert_eq!(beads[1].priority, 0);
    }

    #[test]
    fn bead_deserialize_empty_array() {
        let beads: Vec<Bead> = serde_json::from_str("[]").unwrap();
        assert!(beads.is_empty());
    }

    #[test]
    fn bead_serde_roundtrip() {
        let bead = Bead {
            id: "rt-1".into(),
            title: "Roundtrip".into(),
            description: "test".into(),
            status: "open".into(),
            priority: 3,
            issue_type: "task".into(),
            dependencies: vec![BeadDependency {
                target_id: "rt-0".into(),
                dep_type: "blocks".into(),
            }],
            external_ref: Some("https://example.com".into()),
            labels: vec!["label1".into()],
            assignee: None,
        };
        let json = serde_json::to_string(&bead).unwrap();
        let parsed: Bead = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, bead.id);
        assert_eq!(parsed.dependencies.len(), 1);
        assert_eq!(parsed.dependencies[0].target_id, "rt-0");
    }

    #[test]
    fn bead_dependency_defaults() {
        let json = r#"{"target_id": "dep-1"}"#;
        let dep: BeadDependency = serde_json::from_str(json).unwrap();
        assert_eq!(dep.target_id, "dep-1");
        assert_eq!(dep.dep_type, "");
    }

    #[test]
    fn bead_dependency_bd_json_format() {
        // Actual field names from `bd list --json`
        let json = r#"{
            "issue_id": "gl-1e0.5",
            "depends_on_id": "gl-1e0",
            "type": "blocks",
            "created_at": "2026-02-23T22:20:42Z",
            "created_by": "Kit Plummer"
        }"#;
        let dep: BeadDependency = serde_json::from_str(json).unwrap();
        assert_eq!(dep.target_id, "gl-1e0");
        assert_eq!(dep.dep_type, "blocks");
    }

    #[test]
    fn bead_deserialize_real_bd_output() {
        // Mirrors actual `bd list --json` output structure
        let json = r#"{
            "id": "gl-1e0.5",
            "title": "Pipeline writes bead status on task failure",
            "description": "When a pipeline task fails, write a failure status.",
            "status": "open",
            "priority": 0,
            "issue_type": "task",
            "owner": "kit@example.com",
            "created_at": "2026-02-23T22:20:42Z",
            "created_by": "Kit Plummer",
            "updated_at": "2026-02-23T22:20:42Z",
            "labels": ["beads", "error", "pipeline"],
            "dependencies": [
                {
                    "issue_id": "gl-1e0.5",
                    "depends_on_id": "gl-1e0",
                    "type": "parent-child",
                    "created_at": "2026-02-23T22:20:42Z",
                    "created_by": "Kit Plummer"
                },
                {
                    "issue_id": "gl-1e0.5",
                    "depends_on_id": "gl-1e0.1",
                    "type": "blocks",
                    "created_at": "2026-02-23T22:20:42Z",
                    "created_by": "Kit Plummer"
                }
            ],
            "dependency_count": 2,
            "dependent_count": 0,
            "comment_count": 0
        }"#;
        let bead: Bead = serde_json::from_str(json).unwrap();
        assert_eq!(bead.id, "gl-1e0.5");
        assert_eq!(bead.dependencies.len(), 2);
        assert_eq!(bead.dependencies[0].target_id, "gl-1e0");
        assert_eq!(bead.dependencies[0].dep_type, "parent-child");
        assert_eq!(bead.dependencies[1].target_id, "gl-1e0.1");
        assert_eq!(bead.dependencies[1].dep_type, "blocks");
    }

    // -----------------------------------------------------------------------
    // Arg-capturing mock for verifying bd command signatures
    // -----------------------------------------------------------------------

    /// Create a mock bd script that writes all args to a file and prints
    /// an optional response to stdout.
    fn mock_bd_capture(dir: &std::path::Path, response: &str) -> (PathBuf, PathBuf) {
        use std::io::Write;
        use std::sync::atomic::{AtomicU32, Ordering};
        static CAP_COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = CAP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let script = dir.join(format!("mock_bd_cap_{n}"));
        let args_file = dir.join(format!("mock_bd_args_{n}"));
        let content = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' '{}'\n",
            args_file.display(),
            response.replace('\'', "'\\''")
        );
        let mut f = std::fs::File::create(&script).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.sync_all().unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
        (script, args_file)
    }

    #[tokio::test]
    async fn create_bead_uses_correct_bd_flags() {
        let dir = tempfile::tempdir().unwrap();
        let (script, args_file) = mock_bd_capture(dir.path(), "gl-42");

        let client = BeadsClient::new(dir.path(), Some(script.to_string_lossy().into()));
        let result = client.create_bead("gl-42", "failed").await.unwrap();
        assert_eq!(result, "gl-42");

        let args = std::fs::read_to_string(&args_file).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        // bd create "Task gl-42: failed" --id gl-42 --type task --silent
        assert_eq!(lines[0], "create");
        assert_eq!(lines[1], "Task gl-42: failed");
        assert_eq!(lines[2], "--id");
        assert_eq!(lines[3], "gl-42");
        assert_eq!(lines[4], "--type");
        assert_eq!(lines[5], "task");
        assert_eq!(lines[6], "--silent");
    }

    #[tokio::test]
    async fn update_status_uses_correct_bd_flags() {
        let dir = tempfile::tempdir().unwrap();
        let (script, args_file) = mock_bd_capture(dir.path(), "");

        let client = BeadsClient::new(dir.path(), Some(script.to_string_lossy().into()));
        client.update_status("gl-42", "closed").await.unwrap();

        let args = std::fs::read_to_string(&args_file).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        // bd update gl-42 --status closed
        assert_eq!(lines[0], "update");
        assert_eq!(lines[1], "gl-42");
        assert_eq!(lines[2], "--status");
        assert_eq!(lines[3], "closed");
    }

    #[tokio::test]
    async fn record_updates_existing_bead_first() {
        let dir = tempfile::tempdir().unwrap();
        // Mock always succeeds — update should be tried first
        let (script, args_file) = mock_bd_capture(dir.path(), "");

        let client = BeadsClient::new(dir.path(), Some(script.to_string_lossy().into()));
        let entry = HistoryEntry {
            timestamp: chrono::Utc::now(),
            task_id: "gl-99".into(),
            status: "pr_created".into(),
            pr_url: None,
            branch: None,
            error: None,
            budget: glitchlab_kernel::budget::BudgetSummary::default(),
            events_summary: crate::history::EventsSummary::default(),
            stage_outputs: None,
            events: None,
            outcome_context: None,
        };
        client.record(&entry).await.unwrap();

        let args = std::fs::read_to_string(&args_file).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        // Should have called update (tried first), not create
        // pr_created maps to "closed" via pipeline_status_to_bead_status
        assert_eq!(lines[0], "update");
        assert_eq!(lines[1], "gl-99");
        assert_eq!(lines[2], "--status");
        assert_eq!(lines[3], "closed");
    }

    #[tokio::test]
    async fn close_bead_uses_correct_bd_flags() {
        let dir = tempfile::tempdir().unwrap();
        let (script, args_file) = mock_bd_capture(dir.path(), "");

        let client = BeadsClient::new(dir.path(), Some(script.to_string_lossy().into()));
        client.close_bead("gl-42").await.unwrap();

        let args = std::fs::read_to_string(&args_file).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        // bd close gl-42
        assert_eq!(lines[0], "close");
        assert_eq!(lines[1], "gl-42");
        assert_eq!(lines.len(), 2);
    }

    // -----------------------------------------------------------------------
    // list_parsed / ready_parsed tests (using shell script as mock bd)
    // -----------------------------------------------------------------------

    /// Create a temporary shell script that echoes the given JSON to stdout.
    ///
    /// Uses a unique filename per invocation to avoid "Text file busy" races
    /// when multiple tests share a tempdir or run in parallel.
    fn mock_bd_script(dir: &std::path::Path, json: &str) -> PathBuf {
        use std::io::Write;
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let script = dir.join(format!("mock_bd_{n}"));
        let content = format!("#!/bin/sh\nprintf '%s' '{}'\n", json.replace('\'', "'\\''"));
        let mut f = std::fs::File::create(&script).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.sync_all().unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Brief yield to let the kernel finish closing the file descriptor,
        // preventing ETXTBSY ("Text file busy") on immediate exec.
        std::thread::sleep(std::time::Duration::from_millis(5));
        script
    }

    #[tokio::test]
    async fn list_parsed_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let json =
            r#"[{"id":"t1","title":"Task One"},{"id":"t2","title":"Task Two","priority":0}]"#;
        let script = mock_bd_script(dir.path(), json);

        let client = BeadsClient::new(dir.path(), Some(script.to_string_lossy().into()));
        let beads = client.list_parsed().await.unwrap();
        assert_eq!(beads.len(), 2);
        assert_eq!(beads[0].id, "t1");
        assert_eq!(beads[1].priority, 0);
    }

    #[tokio::test]
    async fn list_parsed_empty_array() {
        let dir = tempfile::tempdir().unwrap();
        let script = mock_bd_script(dir.path(), "[]");

        let client = BeadsClient::new(dir.path(), Some(script.to_string_lossy().into()));
        let beads = client.list_parsed().await.unwrap();
        assert!(beads.is_empty());
    }

    #[tokio::test]
    async fn list_parsed_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let script = mock_bd_script(dir.path(), "not json at all");

        let client = BeadsClient::new(dir.path(), Some(script.to_string_lossy().into()));
        let result = client.list_parsed().await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, MemoryError::Beads(_)));
        assert!(err.to_string().contains("failed to parse bd list"));
    }

    #[tokio::test]
    async fn ready_parsed_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"[{"id":"r1","title":"Ready task"}]"#;
        let script = mock_bd_script(dir.path(), json);

        let client = BeadsClient::new(dir.path(), Some(script.to_string_lossy().into()));
        let beads = client.ready_parsed().await.unwrap();
        assert_eq!(beads.len(), 1);
        assert_eq!(beads[0].id, "r1");
    }

    #[tokio::test]
    async fn ready_parsed_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let script = mock_bd_script(dir.path(), "{bad");

        let client = BeadsClient::new(dir.path(), Some(script.to_string_lossy().into()));
        let result = client.ready_parsed().await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("failed to parse bd ready")
        );
    }

    // -----------------------------------------------------------------------
    // pipeline_status_to_bead_status mapping tests
    // -----------------------------------------------------------------------

    #[test]
    fn status_mapping_success_outcomes() {
        assert_eq!(pipeline_status_to_bead_status("pr_created"), "closed");
        assert_eq!(pipeline_status_to_bead_status("pr_merged"), "closed");
        assert_eq!(pipeline_status_to_bead_status("committed"), "closed");
        assert_eq!(pipeline_status_to_bead_status("already_done"), "closed");
    }

    #[test]
    fn status_mapping_failure_outcomes() {
        for status in &[
            "plan_failed",
            "implementation_failed",
            "tests_failed",
            "parse_error",
            "boundary_violation",
            "architect_rejected",
            "interrupted",
            "budget_exceeded",
            "error",
        ] {
            assert_eq!(
                pipeline_status_to_bead_status(status),
                "open",
                "{status} should map to open"
            );
        }
    }

    #[test]
    fn status_mapping_decomposed() {
        assert_eq!(pipeline_status_to_bead_status("decomposed"), "in_progress");
    }

    #[test]
    fn status_mapping_blocked() {
        assert_eq!(pipeline_status_to_bead_status("blocked"), "blocked");
        assert_eq!(pipeline_status_to_bead_status("escalated"), "blocked");
        assert_eq!(
            pipeline_status_to_bead_status("security_blocked"),
            "blocked"
        );
    }

    #[test]
    fn status_mapping_deferred() {
        assert_eq!(pipeline_status_to_bead_status("deferred"), "deferred");
        assert_eq!(pipeline_status_to_bead_status("retryable"), "deferred");
        assert_eq!(pipeline_status_to_bead_status("timed_out"), "deferred");
    }

    #[test]
    fn status_mapping_unknown() {
        assert_eq!(
            pipeline_status_to_bead_status("something_unexpected"),
            "open"
        );
        assert_eq!(pipeline_status_to_bead_status(""), "open");
    }

    #[tokio::test]
    async fn create_bead_detailed_uses_correct_flags() {
        let dir = tempfile::tempdir().unwrap();
        let (script, args_file) = mock_bd_capture(dir.path(), "adr-test-add-auth");

        let client = BeadsClient::new(dir.path(), Some(script.to_string_lossy().into()));
        let req = BeadCreateRequest {
            id: "adr-test-add-auth".into(),
            title: "Add authentication".into(),
            description: "Implement JWT auth".into(),
            issue_type: "feature".into(),
            priority: 1,
            labels: vec!["adr:adr-test.md".into(), "security".into()],
        };
        let result = client.create_bead_detailed(&req).await.unwrap();
        assert_eq!(result, "adr-test-add-auth");

        let args = std::fs::read_to_string(&args_file).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        // bd create "Add authentication" --id adr-test-add-auth --type feature
        //   --description "Implement JWT auth" --priority 1
        //   --label adr:adr-test.md --label security --silent
        assert_eq!(lines[0], "create");
        assert_eq!(lines[1], "Add authentication");
        assert_eq!(lines[2], "--id");
        assert_eq!(lines[3], "adr-test-add-auth");
        assert_eq!(lines[4], "--type");
        assert_eq!(lines[5], "feature");
        assert_eq!(lines[6], "--description");
        assert_eq!(lines[7], "Implement JWT auth");
        assert_eq!(lines[8], "--priority");
        assert_eq!(lines[9], "1");
        assert_eq!(lines[10], "--label");
        assert_eq!(lines[11], "adr:adr-test.md");
        assert_eq!(lines[12], "--label");
        assert_eq!(lines[13], "security");
        assert_eq!(lines[14], "--silent");
    }

    #[test]
    fn bead_create_request_serde_roundtrip() {
        let req = BeadCreateRequest {
            id: "test-1".into(),
            title: "Test".into(),
            description: "A test bead".into(),
            issue_type: "task".into(),
            priority: 2,
            labels: vec!["adr:test.md".into()],
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: BeadCreateRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "test-1");
        assert_eq!(parsed.labels, vec!["adr:test.md"]);
    }

    #[tokio::test]
    async fn record_uses_mapped_status() {
        // Verify that record() passes the mapped bead status, not the raw
        // pipeline status, to update_status/create_bead.
        let dir = tempfile::tempdir().unwrap();
        let (script, args_file) = mock_bd_capture(dir.path(), "");

        let client = BeadsClient::new(dir.path(), Some(script.to_string_lossy().into()));
        let entry = HistoryEntry {
            timestamp: chrono::Utc::now(),
            task_id: "gl-map".into(),
            status: "tests_failed".into(), // should map to "open"
            pr_url: None,
            branch: None,
            error: None,
            budget: glitchlab_kernel::budget::BudgetSummary::default(),
            events_summary: crate::history::EventsSummary::default(),
            stage_outputs: None,
            events: None,
            outcome_context: None,
        };
        client.record(&entry).await.unwrap();

        let args = std::fs::read_to_string(&args_file).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        assert_eq!(lines[0], "update");
        assert_eq!(lines[1], "gl-map");
        assert_eq!(lines[2], "--status");
        assert_eq!(lines[3], "open"); // mapped, not "tests_failed"
    }
}
