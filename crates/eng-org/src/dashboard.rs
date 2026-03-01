//! Dashboard event emitter for GLITCHLAB.
//!
//! Emits structured, high-level events to a JSONL file and to `tracing` (for
//! OpenTelemetry export). Events use `target: "glitchlab::dashboard"` so an
//! OTel layer can filter them from debug noise.
//!
//! # OpenTelemetry integration
//!
//! Add `tracing-opentelemetry` as a subscriber layer — every dashboard event
//! becomes an OTel LogRecord with structured attributes. No code changes
//! needed in this module.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;
use tracing::info;

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

/// Structured dashboard event.
///
/// Serialized to JSONL with `#[serde(tag = "event")]` so each line is
/// self-describing: `{"event":"pr_created","task_id":"foo",...}`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DashboardEvent {
    /// Orchestrator run started.
    RunStarted {
        total_tasks: usize,
        budget_dollars: f64,
    },
    /// Orchestrator run completed.
    RunCompleted {
        succeeded: u32,
        failed: u32,
        deferred: u32,
        escalated: u32,
        total_cost: f64,
        duration_secs: f64,
        cease_reason: String,
    },
    /// Task picked up from queue.
    TaskStarted {
        task_id: String,
        is_remediation: bool,
    },
    /// Task finished successfully (PR created, committed, merged, or already done).
    TaskSucceeded {
        task_id: String,
        status: String,
        cost: f64,
        tokens: u64,
        pr_url: Option<String>,
    },
    /// Task failed.
    TaskFailed {
        task_id: String,
        status: String,
        cost: f64,
        reason: Option<String>,
    },
    /// Task decomposed into sub-tasks.
    TaskDecomposed {
        task_id: String,
        sub_task_ids: Vec<String>,
    },
    /// Pull request created.
    PrCreated { task_id: String, url: String },
    /// Pull request merged.
    PrMerged { task_id: String, url: String },
    /// Boundary violation — task hit a protected path.
    BoundaryViolation { task_id: String },
    /// Budget threshold crossed (emitted at 50%, 75%, 90%).
    BudgetThreshold {
        pct_used: f64,
        remaining: f64,
        total: f64,
    },
    /// TQM detected an anti-pattern.
    PatternDetected {
        kind: String,
        severity: String,
        occurrences: u32,
    },
    /// Remediation tasks injected into queue.
    RemediationInjected {
        source: String,
        count: usize,
        task_ids: Vec<String>,
    },
    /// Circuit agent escalated — needs human decision.
    CircuitEscalation { reason: String },
    /// Quality gate passed between tasks.
    QualityGatePassed,
    /// Quality gate failed — orchestrator halting.
    QualityGateFailed { details: String },
    /// Systemic failure detected — orchestrator halting.
    SystemicFailure { dominant_category: String },
    /// Pre-batch backlog review started.
    BacklogReviewStarted,
    /// Pre-batch backlog review completed.
    BacklogReviewCompleted {
        beads_reviewed: usize,
        actions_applied: usize,
        cost: f64,
    },
}

// ---------------------------------------------------------------------------
// JSONL record (timestamp wrapper)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Record<'a> {
    ts: String,
    #[serde(flatten)]
    event: &'a DashboardEvent,
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// Writes dashboard events to a JSONL file and emits them as `tracing` events.
///
/// Thread-safe via internal `Mutex`. Cheap to clone via `Arc`.
pub struct DashboardEmitter {
    writer: Mutex<Box<dyn Write + Send>>,
    /// Budget thresholds already emitted (to avoid duplicates).
    emitted_thresholds: Mutex<Vec<u8>>,
}

impl DashboardEmitter {
    /// Create an emitter that writes to the given file path.
    ///
    /// Creates parent directories if needed. Appends to existing file.
    pub fn new(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            writer: Mutex::new(Box::new(std::io::BufWriter::new(file))),
            emitted_thresholds: Mutex::new(Vec::new()),
        })
    }

    /// Create an emitter that writes to an in-memory buffer (for testing).
    #[cfg(test)]
    pub fn in_memory() -> (Self, std::sync::Arc<Mutex<Vec<u8>>>) {
        let buf = std::sync::Arc::new(Mutex::new(Vec::new()));
        let writer = {
            let buf = buf.clone();
            SharedBuf(buf)
        };
        (
            Self {
                writer: Mutex::new(Box::new(writer)),
                emitted_thresholds: Mutex::new(Vec::new()),
            },
            buf,
        )
    }

    /// Emit a dashboard event.
    ///
    /// Writes a JSONL line to the file and emits a `tracing::info!` event
    /// with `target: "glitchlab::dashboard"` for OTel export.
    pub fn emit(&self, event: DashboardEvent) {
        // Write JSONL
        let record = Record {
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            event: &event,
        };
        if let Ok(mut writer) = self.writer.lock()
            && let Ok(json) = serde_json::to_string(&record)
        {
            let _ = writeln!(writer, "{json}");
            let _ = writer.flush();
        }

        // Emit tracing event for OTel
        self.emit_tracing(&event);
    }

    /// Check budget and emit threshold events at 50%, 75%, 90%.
    pub fn check_budget_threshold(&self, spent: f64, total: f64) {
        if total <= 0.0 {
            return;
        }
        let pct = (spent / total * 100.0) as u8;
        let thresholds = [50, 75, 90];

        let mut emitted = self
            .emitted_thresholds
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for &t in &thresholds {
            if pct >= t && !emitted.contains(&t) {
                emitted.push(t);
                self.emit(DashboardEvent::BudgetThreshold {
                    pct_used: pct as f64,
                    remaining: total - spent,
                    total,
                });
            }
        }
    }

    /// Emit a tracing event with structured fields.
    ///
    /// Uses `target: "glitchlab::dashboard"` so OTel layers can filter.
    fn emit_tracing(&self, event: &DashboardEvent) {
        match event {
            DashboardEvent::RunStarted {
                total_tasks,
                budget_dollars,
            } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "run_started",
                    total_tasks,
                    budget_dollars,
                );
            }
            DashboardEvent::RunCompleted {
                succeeded,
                failed,
                deferred,
                escalated,
                total_cost,
                duration_secs,
                cease_reason,
            } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "run_completed",
                    succeeded,
                    failed,
                    deferred,
                    escalated,
                    total_cost,
                    duration_secs,
                    cease_reason = cease_reason.as_str(),
                );
            }
            DashboardEvent::TaskStarted {
                task_id,
                is_remediation,
            } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "task_started",
                    task_id = task_id.as_str(),
                    is_remediation,
                );
            }
            DashboardEvent::TaskSucceeded {
                task_id,
                status,
                cost,
                tokens,
                pr_url,
            } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "task_succeeded",
                    task_id = task_id.as_str(),
                    status = status.as_str(),
                    cost,
                    tokens,
                    pr_url = pr_url.as_deref().unwrap_or(""),
                );
            }
            DashboardEvent::TaskFailed {
                task_id,
                status,
                cost,
                reason,
            } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "task_failed",
                    task_id = task_id.as_str(),
                    status = status.as_str(),
                    cost,
                    reason = reason.as_deref().unwrap_or(""),
                );
            }
            DashboardEvent::TaskDecomposed {
                task_id,
                sub_task_ids,
            } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "task_decomposed",
                    task_id = task_id.as_str(),
                    sub_task_count = sub_task_ids.len(),
                );
            }
            DashboardEvent::PrCreated { task_id, url } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "pr_created",
                    task_id = task_id.as_str(),
                    url = url.as_str(),
                );
            }
            DashboardEvent::PrMerged { task_id, url } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "pr_merged",
                    task_id = task_id.as_str(),
                    url = url.as_str(),
                );
            }
            DashboardEvent::BoundaryViolation { task_id } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "boundary_violation",
                    task_id = task_id.as_str(),
                );
            }
            DashboardEvent::BudgetThreshold {
                pct_used,
                remaining,
                total,
            } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "budget_threshold",
                    pct_used,
                    remaining,
                    total,
                );
            }
            DashboardEvent::PatternDetected {
                kind,
                severity,
                occurrences,
            } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "pattern_detected",
                    kind = kind.as_str(),
                    severity = severity.as_str(),
                    occurrences,
                );
            }
            DashboardEvent::RemediationInjected {
                source,
                count,
                task_ids,
            } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "remediation_injected",
                    source = source.as_str(),
                    count,
                    task_ids = ?task_ids,
                );
            }
            DashboardEvent::CircuitEscalation { reason } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "circuit_escalation",
                    reason = reason.as_str(),
                );
            }
            DashboardEvent::QualityGatePassed => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "quality_gate_passed",
                );
            }
            DashboardEvent::QualityGateFailed { details } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "quality_gate_failed",
                    details = details.as_str(),
                );
            }
            DashboardEvent::SystemicFailure { dominant_category } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "systemic_failure",
                    dominant_category = dominant_category.as_str(),
                );
            }
            DashboardEvent::BacklogReviewStarted => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "backlog_review_started",
                );
            }
            DashboardEvent::BacklogReviewCompleted {
                beads_reviewed,
                actions_applied,
                cost,
            } => {
                info!(
                    target: "glitchlab::dashboard",
                    event = "backlog_review_completed",
                    beads_reviewed,
                    actions_applied,
                    cost,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

#[cfg(test)]
struct SharedBuf(std::sync::Arc<Mutex<Vec<u8>>>);

#[cfg(test)]
impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emitter_new_creates_file_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events/dashboard.jsonl");
        let emitter = DashboardEmitter::new(&path).unwrap();
        emitter.emit(DashboardEvent::RunStarted {
            total_tasks: 5,
            budget_dollars: 10.0,
        });

        let content = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(v["event"], "run_started");
        assert_eq!(v["total_tasks"], 5);
    }

    #[test]
    fn emit_writes_jsonl() {
        let (emitter, buf) = DashboardEmitter::in_memory();
        emitter.emit(DashboardEvent::RunStarted {
            total_tasks: 10,
            budget_dollars: 5.0,
        });
        emitter.emit(DashboardEvent::PrCreated {
            task_id: "task-1".into(),
            url: "https://github.com/example/repo/pull/1".into(),
        });

        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(lines.len(), 2);

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["event"], "run_started");
        assert_eq!(first["total_tasks"], 10);
        assert!(first["ts"].as_str().is_some());

        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["event"], "pr_created");
        assert_eq!(second["task_id"], "task-1");
    }

    #[test]
    fn budget_thresholds_emitted_once() {
        let (emitter, buf) = DashboardEmitter::in_memory();

        // Cross 50%
        emitter.check_budget_threshold(5.5, 10.0);
        // Still at 55% — should not emit again
        emitter.check_budget_threshold(5.6, 10.0);
        // Cross 75%
        emitter.check_budget_threshold(7.5, 10.0);
        // Cross 90%
        emitter.check_budget_threshold(9.1, 10.0);

        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(lines.len(), 3, "should emit exactly 3 threshold events");

        let pcts: Vec<f64> = lines
            .iter()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["pct_used"].as_f64().unwrap()
            })
            .collect();
        assert!(pcts[0] >= 50.0);
        assert!(pcts[1] >= 75.0);
        assert!(pcts[2] >= 90.0);
    }

    #[test]
    fn all_event_variants_serialize() {
        let events = vec![
            DashboardEvent::RunStarted {
                total_tasks: 5,
                budget_dollars: 100.0,
            },
            DashboardEvent::RunCompleted {
                succeeded: 3,
                failed: 1,
                deferred: 1,
                escalated: 0,
                total_cost: 42.0,
                duration_secs: 120.0,
                cease_reason: "all_tasks_done".into(),
            },
            DashboardEvent::TaskStarted {
                task_id: "t1".into(),
                is_remediation: false,
            },
            DashboardEvent::TaskSucceeded {
                task_id: "t1".into(),
                status: "PrCreated".into(),
                cost: 0.35,
                tokens: 5000,
                pr_url: Some("https://example.com/pr/1".into()),
            },
            DashboardEvent::TaskFailed {
                task_id: "t2".into(),
                status: "ImplementationFailed".into(),
                cost: 0.20,
                reason: Some("tests failed".into()),
            },
            DashboardEvent::TaskDecomposed {
                task_id: "t3".into(),
                sub_task_ids: vec!["t3-a".into(), "t3-b".into()],
            },
            DashboardEvent::PrCreated {
                task_id: "t1".into(),
                url: "https://example.com/pr/1".into(),
            },
            DashboardEvent::PrMerged {
                task_id: "t1".into(),
                url: "https://example.com/pr/1".into(),
            },
            DashboardEvent::BoundaryViolation {
                task_id: "t4".into(),
            },
            DashboardEvent::BudgetThreshold {
                pct_used: 75.0,
                remaining: 25.0,
                total: 100.0,
            },
            DashboardEvent::PatternDetected {
                kind: "stuck_agents".into(),
                severity: "warning".into(),
                occurrences: 3,
            },
            DashboardEvent::RemediationInjected {
                source: "circuit".into(),
                count: 2,
                task_ids: vec!["fix-1".into(), "fix-2".into()],
            },
            DashboardEvent::CircuitEscalation {
                reason: "kernel changes needed".into(),
            },
            DashboardEvent::QualityGatePassed,
            DashboardEvent::QualityGateFailed {
                details: "cargo test failed".into(),
            },
            DashboardEvent::SystemicFailure {
                dominant_category: "provider_failures".into(),
            },
            DashboardEvent::BacklogReviewStarted,
            DashboardEvent::BacklogReviewCompleted {
                beads_reviewed: 5,
                actions_applied: 2,
                cost: 0.01,
            },
        ];

        let (emitter, buf) = DashboardEmitter::in_memory();
        for event in &events {
            emitter.emit(event.clone());
        }

        let output = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(lines.len(), events.len());

        // Every line must be valid JSON with a "ts" and "event" field.
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v["ts"].is_string());
            assert!(v["event"].is_string());
        }
    }
}
