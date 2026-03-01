use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use chrono::{DateTime, Utc};
use glitchlab_kernel::budget::BudgetSummary;
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::Result;

// ---------------------------------------------------------------------------
// History entry — what gets stored per task run
// ---------------------------------------------------------------------------

/// A single task execution record, appended to history.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    #[serde(deserialize_with = "deserialize_timestamp")]
    pub timestamp: DateTime<Utc>,
    pub task_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub budget: BudgetSummary,
    #[serde(default)]
    pub events_summary: EventsSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_outputs: Option<std::collections::HashMap<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_context: Option<glitchlab_kernel::outcome::OutcomeContext>,
}

/// Backward-compatible timestamp deserializer: accepts both `DateTime<Utc>`
/// and plain RFC 3339 strings (the old format).
fn deserialize_timestamp<'de, D>(deserializer: D) -> std::result::Result<DateTime<Utc>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match &value {
        serde_json::Value::String(s) => {
            // Try chrono's default parse first, then RFC 3339.
            s.parse::<DateTime<Utc>>()
                .or_else(|_| DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc)))
                .map_err(serde::de::Error::custom)
        }
        _ => Err(serde::de::Error::custom("expected string for timestamp")),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventsSummary {
    #[serde(default)]
    pub plan_steps: u32,
    #[serde(default)]
    pub plan_risk: String,
    #[serde(default)]
    pub tests_passed_on_attempt: u32,
    #[serde(default)]
    pub security_verdict: String,
    #[serde(default)]
    pub version_bump: String,
    #[serde(default)]
    pub fix_attempts: u32,
}

/// Event recorded when a model is escalated to a more powerful one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEscalation {
    pub original_model: String,
    pub escalated_model: String,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// HistoryQuery — filter/limit for reads
// ---------------------------------------------------------------------------

/// Query parameters for reading history entries.
#[derive(Debug, Clone)]
pub struct HistoryQuery {
    pub limit: usize,
    pub failures_only: bool,
    pub task_id: Option<String>,
    pub since: Option<DateTime<Utc>>,
}

impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            limit: 50,
            failures_only: false,
            task_id: None,
            since: None,
        }
    }
}

// ---------------------------------------------------------------------------
// HistoryStats
// ---------------------------------------------------------------------------

/// Summary statistics for the task history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryStats {
    pub total_runs: usize,
    pub successes: usize,
    pub failures: usize,
    pub total_cost: f64,
    pub total_tokens: u64,
}

// ---------------------------------------------------------------------------
// HistoryBackend trait — async, object-safe
// ---------------------------------------------------------------------------

/// Async persistence backend for task history.
///
/// Object-safe via `Pin<Box<...>>` return types.
pub trait HistoryBackend: Send + Sync {
    /// Record a completed task run.
    fn record<'a>(
        &'a self,
        entry: &'a HistoryEntry,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

    /// Query history entries with filters.
    fn query<'a>(
        &'a self,
        query: &'a HistoryQuery,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<HistoryEntry>>> + Send + 'a>>;

    /// Compute summary statistics.
    fn stats(&self) -> Pin<Box<dyn Future<Output = Result<HistoryStats>> + Send + '_>>;

    /// Build a context string from recent failures for planner injection.
    fn failure_context(
        &self,
        max_entries: usize,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>>;

    /// Human-readable backend name.
    fn backend_name(&self) -> &str;

    /// Check if the backend is available and ready.
    fn is_available(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;
}

// ---------------------------------------------------------------------------
// JsonlHistory — async JSONL append-only history backend
// ---------------------------------------------------------------------------

/// Append-only JSONL task history (async).
///
/// This is the fallback backend that works without Dolt.
/// When Dolt is available, history is also written to SQL tables,
/// but the JSONL file is always maintained as the ground truth.
pub struct JsonlHistory {
    history_file: PathBuf,
}

/// Deprecated: use `JsonlHistory` instead.
#[deprecated(note = "renamed to JsonlHistory")]
pub type TaskHistory = JsonlHistory;

impl JsonlHistory {
    pub fn new(repo_path: &Path) -> Self {
        let log_dir = repo_path.join(".glitchlab").join("logs");
        Self {
            history_file: log_dir.join("history.jsonl"),
        }
    }

    /// Read all entries from the JSONL file, gracefully skipping malformed lines.
    async fn read_all(&self) -> Result<Vec<HistoryEntry>> {
        if !self.history_file.exists() {
            return Ok(Vec::new());
        }

        let contents = tokio::fs::read_to_string(&self.history_file).await?;
        let mut entries = Vec::new();
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<HistoryEntry>(line) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    tracing::warn!(error = %e, "skipping malformed history line");
                }
            }
        }
        Ok(entries)
    }

    fn is_success(status: &str) -> bool {
        matches!(status, "pr_created" | "committed")
    }
}

impl HistoryBackend for JsonlHistory {
    fn record<'a>(
        &'a self,
        entry: &'a HistoryEntry,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(parent) = self.history_file.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            let line = serde_json::to_string(entry)?;
            let mut content = line;
            content.push('\n');

            tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.history_file)
                .await?;

            // Append atomically via write-all.
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.history_file)
                .await?;
            file.write_all(content.as_bytes()).await?;
            file.flush().await?;
            Ok(())
        })
    }

    fn query<'a>(
        &'a self,
        query: &'a HistoryQuery,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<HistoryEntry>>> + Send + 'a>> {
        Box::pin(async move {
            let entries = self.read_all().await?;
            let filtered = entries
                .into_iter()
                .rev()
                .filter(|e| {
                    if query.failures_only && Self::is_success(&e.status) {
                        return false;
                    }
                    if let Some(ref tid) = query.task_id
                        && e.task_id != *tid
                    {
                        return false;
                    }
                    if let Some(since) = query.since
                        && e.timestamp < since
                    {
                        return false;
                    }
                    true
                })
                .take(query.limit)
                .collect();
            Ok(filtered)
        })
    }

    fn stats(&self) -> Pin<Box<dyn Future<Output = Result<HistoryStats>> + Send + '_>> {
        Box::pin(async {
            let entries = self.read_all().await?;
            let total = entries.len();
            let successes = entries
                .iter()
                .filter(|e| Self::is_success(&e.status))
                .count();
            let failures = total - successes;
            let total_cost: f64 = entries.iter().map(|e| e.budget.estimated_cost).sum();
            let total_tokens: u64 = entries.iter().map(|e| e.budget.total_tokens).sum();

            Ok(HistoryStats {
                total_runs: total,
                successes,
                failures,
                total_cost,
                total_tokens,
            })
        })
    }

    fn failure_context(
        &self,
        max_entries: usize,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        Box::pin(async move {
            let query = HistoryQuery {
                limit: max_entries,
                failures_only: true,
                ..Default::default()
            };
            let failures = self.query(&query).await?;
            if failures.is_empty() {
                return Ok(String::new());
            }

            let mut context = String::from("Recent failures to avoid repeating:\n");
            for entry in &failures {
                context.push_str(&format!(
                    "- Task `{}` failed with status `{}`",
                    entry.task_id, entry.status
                ));
                if let Some(err) = &entry.error {
                    context.push_str(&format!(": {err}"));
                }
                context.push('\n');
            }
            Ok(context)
        })
    }

    fn backend_name(&self) -> &str {
        "jsonl"
    }

    fn is_available(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async { true })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, TimeZone};
    use std::io::Write;

    fn temp_history() -> (tempfile::TempDir, JsonlHistory) {
        let dir = tempfile::tempdir().unwrap();
        let history = JsonlHistory::new(dir.path());
        (dir, history)
    }

    fn sample_entry(task_id: &str, status: &str) -> HistoryEntry {
        HistoryEntry {
            timestamp: Utc.with_ymd_and_hms(2026, 2, 21, 14, 30, 0).unwrap(),
            task_id: task_id.into(),
            status: status.into(),
            pr_url: None,
            branch: Some(format!("glitchlab/{task_id}")),
            error: if status == "error" {
                Some("something broke".into())
            } else {
                None
            },
            budget: BudgetSummary {
                total_tokens: 5000,
                estimated_cost: 0.50,
                call_count: 3,
                tokens_remaining: 145_000,
                dollars_remaining: 9.50,
            },
            events_summary: EventsSummary::default(),
            stage_outputs: None,
            events: None,
            outcome_context: None,
        }
    }

    #[tokio::test]
    async fn record_and_read() {
        let (_dir, history) = temp_history();
        history
            .record(&sample_entry("task-1", "pr_created"))
            .await
            .unwrap();
        history
            .record(&sample_entry("task-2", "error"))
            .await
            .unwrap();

        let query = HistoryQuery {
            limit: 10,
            ..Default::default()
        };
        let recent = history.query(&query).await.unwrap();
        assert_eq!(recent.len(), 2);
        // Most recent first.
        assert_eq!(recent[0].task_id, "task-2");
    }

    #[tokio::test]
    async fn failures_only() {
        let (_dir, history) = temp_history();
        history
            .record(&sample_entry("task-1", "pr_created"))
            .await
            .unwrap();
        history
            .record(&sample_entry("task-2", "error"))
            .await
            .unwrap();
        history
            .record(&sample_entry("task-3", "tests_failed"))
            .await
            .unwrap();
        history
            .record(&sample_entry("task-4", "committed"))
            .await
            .unwrap();

        let query = HistoryQuery {
            limit: 10,
            failures_only: true,
            ..Default::default()
        };
        let failures = history.query(&query).await.unwrap();
        assert_eq!(failures.len(), 2);
    }

    #[tokio::test]
    async fn failure_context_string() {
        let (_dir, history) = temp_history();
        history
            .record(&sample_entry("task-1", "error"))
            .await
            .unwrap();

        let ctx = history.failure_context(5).await.unwrap();
        assert!(ctx.contains("task-1"));
        assert!(ctx.contains("something broke"));
    }

    #[tokio::test]
    async fn empty_history() {
        let (_dir, history) = temp_history();
        let query = HistoryQuery {
            limit: 10,
            ..Default::default()
        };
        assert!(history.query(&query).await.unwrap().is_empty());
        assert!(history.failure_context(5).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn stats() {
        let (_dir, history) = temp_history();
        history
            .record(&sample_entry("task-1", "pr_created"))
            .await
            .unwrap();
        history
            .record(&sample_entry("task-2", "error"))
            .await
            .unwrap();
        history
            .record(&sample_entry("task-3", "pr_created"))
            .await
            .unwrap();

        let stats = history.stats().await.unwrap();
        assert_eq!(stats.total_runs, 3);
        assert_eq!(stats.successes, 2);
        assert_eq!(stats.failures, 1);
        assert!((stats.total_cost - 1.50).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn backward_compat_string_timestamp() {
        // Old format: plain string timestamp. Should deserialize correctly.
        let json = r#"{"timestamp":"2026-02-21T14:30:00Z","task_id":"old","status":"pr_created","budget":{},"events_summary":{}}"#;
        let entry: HistoryEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.task_id, "old");
        assert_eq!(entry.timestamp.year(), 2026);
    }

    #[tokio::test]
    async fn backward_compat_rfc3339_with_offset() {
        let json = r#"{"timestamp":"2026-02-21T14:30:00+00:00","task_id":"rfc","status":"ok","budget":{},"events_summary":{}}"#;
        let entry: HistoryEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.task_id, "rfc");
    }

    #[test]
    fn history_query_defaults() {
        let q = HistoryQuery::default();
        assert_eq!(q.limit, 50);
        assert!(!q.failures_only);
        assert!(q.task_id.is_none());
        assert!(q.since.is_none());
    }

    #[tokio::test]
    async fn query_by_task_id() {
        let (_dir, history) = temp_history();
        history
            .record(&sample_entry("task-1", "pr_created"))
            .await
            .unwrap();
        history
            .record(&sample_entry("task-2", "error"))
            .await
            .unwrap();
        history
            .record(&sample_entry("task-1", "committed"))
            .await
            .unwrap();

        let query = HistoryQuery {
            limit: 10,
            task_id: Some("task-1".into()),
            ..Default::default()
        };
        let results = history.query(&query).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|e| e.task_id == "task-1"));
    }

    #[tokio::test]
    async fn query_since() {
        let (_dir, history) = temp_history();
        let mut old = sample_entry("task-old", "pr_created");
        old.timestamp = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        history.record(&old).await.unwrap();

        let mut new = sample_entry("task-new", "pr_created");
        new.timestamp = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        history.record(&new).await.unwrap();

        let query = HistoryQuery {
            limit: 10,
            since: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
            ..Default::default()
        };
        let results = history.query(&query).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].task_id, "task-new");
    }

    #[tokio::test]
    async fn is_available_returns_true() {
        let (_dir, history) = temp_history();
        assert!(history.is_available().await);
    }

    #[test]
    fn backend_name_is_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let history = JsonlHistory::new(dir.path());
        assert_eq!(history.backend_name(), "jsonl");
    }

    #[tokio::test]
    async fn deprecated_type_alias_compiles() {
        #[allow(deprecated)]
        let _history: TaskHistory = JsonlHistory::new(Path::new("/tmp/test"));
        // Just checking it compiles.
    }

    #[tokio::test]
    async fn serde_roundtrip_with_chrono_timestamp() {
        let entry = sample_entry("rt", "pr_created");
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: HistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.task_id, "rt");
        assert_eq!(parsed.timestamp, entry.timestamp);
    }

    #[tokio::test]
    async fn entry_with_stage_outputs_and_events() {
        let mut entry = sample_entry("rich", "pr_created");
        entry.stage_outputs = Some(std::collections::HashMap::from([(
            "plan".into(),
            serde_json::json!({"steps": []}),
        )]));
        entry.events = Some(vec![serde_json::json!({"kind": "PlanCreated"})]);

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: HistoryEntry = serde_json::from_str(&json).unwrap();
        assert!(parsed.stage_outputs.is_some());
        assert!(parsed.events.is_some());
    }

    #[test]
    fn entry_with_outcome_context_roundtrip() {
        use glitchlab_kernel::outcome::{ObstacleKind, OutcomeContext};

        let mut entry = sample_entry("ctx-task", "implementation_failed");
        entry.outcome_context = Some(OutcomeContext {
            approach: "added greet fn".into(),
            obstacle: ObstacleKind::TestFailure {
                attempts: 3,
                last_error: "assertion failed".into(),
            },
            discoveries: vec!["uses no_std".into()],
            recommendation: Some("use core::fmt".into()),
            files_explored: vec!["src/lib.rs".into()],
        });

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("outcome_context"));
        let parsed: HistoryEntry = serde_json::from_str(&json).unwrap();
        let oc = parsed.outcome_context.unwrap();
        assert_eq!(oc.approach, "added greet fn");
        assert_eq!(oc.discoveries.len(), 1);
    }

    #[test]
    fn entry_without_outcome_context_backward_compat() {
        // Old history entries won't have outcome_context — should deserialize fine.
        let json = r#"{"timestamp":"2026-02-21T14:30:00Z","task_id":"old","status":"error","budget":{},"events_summary":{}}"#;
        let entry: HistoryEntry = serde_json::from_str(json).unwrap();
        assert!(entry.outcome_context.is_none());
    }

    #[tokio::test]
    async fn malformed_jsonl_line_is_skipped() {
        let (_dir, history) = temp_history();
        // Write a valid entry, then a malformed line, then another valid entry.
        history
            .record(&sample_entry("task-1", "pr_created"))
            .await
            .unwrap();

        // Append a malformed line directly to the file.
        let path = _dir.path().join(".glitchlab/logs/history.jsonl");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"this is not valid json\n")
            .unwrap();

        history
            .record(&sample_entry("task-2", "error"))
            .await
            .unwrap();

        let query = HistoryQuery {
            limit: 10,
            ..Default::default()
        };
        let entries = history.query(&query).await.unwrap();
        // The malformed line should be silently skipped.
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn timestamp_non_string_errors() {
        let json =
            r#"{"timestamp":12345,"task_id":"t","status":"ok","budget":{},"events_summary":{}}"#;
        let result = serde_json::from_str::<HistoryEntry>(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("expected string"));
    }

    #[test]
    fn model_escalation_roundtrip() {
        let event = ModelEscalation {
            original_model: "claude-3-opus-20240229".into(),
            escalated_model: "gpt-4-turbo".into(),
            reason: "model lacked capability".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: ModelEscalation = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.original_model, "claude-3-opus-20240229");
        assert_eq!(parsed.escalated_model, "gpt-4-turbo");
        assert_eq!(parsed.reason, "model lacked capability");
    }
}
