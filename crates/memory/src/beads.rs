//! Beads (task graph) backend via the `bd` CLI subprocess.
//!
//! Beads is write-only for history purposes; reads go through Dolt or JSONL.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::error::{MemoryError, Result};
use crate::history::{HistoryBackend, HistoryEntry, HistoryQuery, HistoryStats};

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
        self.run_bd(&["create", "--task", task_id, "--status", status])
            .await
    }

    /// Link two beads.
    pub async fn link(&self, from: &str, to: &str) -> Result<String> {
        self.run_bd(&["link", from, to]).await
    }

    /// Update a bead's status.
    pub async fn update_status(&self, task_id: &str, status: &str) -> Result<String> {
        self.run_bd(&["update", "--task", task_id, "--status", status])
            .await
    }

    /// List beads.
    pub async fn list(&self) -> Result<String> {
        self.run_bd(&["list"]).await
    }
}

impl HistoryBackend for BeadsClient {
    fn record<'a>(
        &'a self,
        entry: &'a HistoryEntry,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            self.create_bead(&entry.task_id, &entry.status)
                .await
                .map(|_| ())
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
}
