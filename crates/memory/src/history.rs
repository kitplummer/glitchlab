use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use glitchlab_kernel::error::{Error, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// History entry — what gets stored per task run
// ---------------------------------------------------------------------------

/// A single task execution record, appended to history.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub timestamp: String,
    pub task_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub budget: BudgetSnapshot,
    #[serde(default)]
    pub events_summary: EventsSummary,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub estimated_cost: f64,
    #[serde(default)]
    pub call_count: u64,
    #[serde(default)]
    pub tokens_remaining: u64,
    #[serde(default)]
    pub dollars_remaining: f64,
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

// ---------------------------------------------------------------------------
// TaskHistory — JSONL append-only history backend
// ---------------------------------------------------------------------------

/// Append-only JSONL task history.
///
/// This is the fallback backend that works without Dolt.
/// When Dolt is available, history is also written to SQL tables,
/// but the JSONL file is always maintained as the ground truth.
pub struct TaskHistory {
    history_file: PathBuf,
}

impl TaskHistory {
    pub fn new(repo_path: &Path) -> Self {
        let log_dir = repo_path.join(".glitchlab").join("logs");
        Self {
            history_file: log_dir.join("history.jsonl"),
        }
    }

    /// Record a completed task run.
    pub fn record(&self, entry: &HistoryEntry) -> Result<()> {
        if let Some(parent) = self.history_file.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_file)?;

        let line = serde_json::to_string(entry)
            .map_err(|e| Error::Config(format!("failed to serialize history entry: {e}")))?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// Read the last N entries.
    pub fn get_recent(&self, n: usize) -> Result<Vec<HistoryEntry>> {
        let entries = self.read_all()?;
        Ok(entries.into_iter().rev().take(n).collect())
    }

    /// Read recent failures only.
    pub fn get_failures(&self, n: usize) -> Result<Vec<HistoryEntry>> {
        let entries = self.read_all()?;
        Ok(entries
            .into_iter()
            .rev()
            .filter(|e| !matches!(e.status.as_str(), "pr_created" | "committed"))
            .take(n)
            .collect())
    }

    /// Build a context string from recent failures for planner injection.
    pub fn build_failure_context(&self, max_entries: usize) -> Result<String> {
        let failures = self.get_failures(max_entries)?;
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
    }

    /// Compute summary statistics.
    pub fn get_stats(&self) -> Result<HistoryStats> {
        let entries = self.read_all()?;
        let total = entries.len();
        let successes = entries
            .iter()
            .filter(|e| matches!(e.status.as_str(), "pr_created" | "committed"))
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
    }

    fn read_all(&self) -> Result<Vec<HistoryEntry>> {
        if !self.history_file.exists() {
            return Ok(Vec::new());
        }

        let file = fs::File::open(&self.history_file)?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<HistoryEntry>(&line) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    tracing::warn!(error = %e, "skipping malformed history line");
                }
            }
        }
        Ok(entries)
    }
}

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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use std::fs;

    fn temp_history() -> (tempfile::TempDir, TaskHistory) {
        let dir = tempfile::tempdir().unwrap();
        let history = TaskHistory::new(dir.path());
        (dir, history)
    }

    fn sample_entry(task_id: &str, status: &str) -> HistoryEntry {
        HistoryEntry {
            timestamp: "2026-02-21T14:30:00Z".into(),
            task_id: task_id.into(),
            status: status.into(),
            pr_url: None,
            branch: Some(format!("glitchlab/{task_id}")),
            error: if status == "error" {
                Some("something broke".into())
            } else {
                None
            },
            budget: BudgetSnapshot {
                total_tokens: 5000,
                estimated_cost: 0.50,
                call_count: 3,
                tokens_remaining: 145_000,
                dollars_remaining: 9.50,
            },
            events_summary: EventsSummary::default(),
        }
    }

    #[test]
    fn record_and_read() {
        let (_dir, history) = temp_history();
        history.record(&sample_entry("task-1", "pr_created")).unwrap();
        history.record(&sample_entry("task-2", "error")).unwrap();

        let recent = history.get_recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        // Most recent first.
        assert_eq!(recent[0].task_id, "task-2");
    }

    #[test]
    fn failures_only() {
        let (_dir, history) = temp_history();
        history.record(&sample_entry("task-1", "pr_created")).unwrap();
        history.record(&sample_entry("task-2", "error")).unwrap();
        history.record(&sample_entry("task-3", "tests_failed")).unwrap();
        history.record(&sample_entry("task-4", "committed")).unwrap();

        let failures = history.get_failures(10).unwrap();
        assert_eq!(failures.len(), 2);
    }

    #[test]
    fn failure_context_string() {
        let (_dir, history) = temp_history();
        history.record(&sample_entry("task-1", "error")).unwrap();

        let ctx = history.build_failure_context(5).unwrap();
        assert!(ctx.contains("task-1"));
        assert!(ctx.contains("something broke"));
    }

    #[test]
    fn empty_history() {
        let (_dir, history) = temp_history();
        assert!(history.get_recent(10).unwrap().is_empty());
        assert!(history.build_failure_context(5).unwrap().is_empty());
    }

    #[test]
    fn stats() {
        let (_dir, history) = temp_history();
        history.record(&sample_entry("task-1", "pr_created")).unwrap();
        history.record(&sample_entry("task-2", "error")).unwrap();
        history.record(&sample_entry("task-3", "pr_created")).unwrap();

        let stats = history.get_stats().unwrap();
        assert_eq!(stats.total_runs, 3);
        assert_eq!(stats.successes, 2);
        assert_eq!(stats.failures, 1);
        assert!((stats.total_cost - 1.50).abs() < f64::EPSILON);
    }
}
