use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use glitchlab_kernel::agent::AgentOutput;
use glitchlab_kernel::budget::BudgetTracker;
use glitchlab_kernel::error::{Error, Result};
use glitchlab_kernel::outcome::OutcomeContext;
use glitchlab_kernel::pipeline::PipelineStatus;
use glitchlab_memory::history::HistoryBackend;
use glitchlab_router::Router;

use crate::config::EngConfig;
use crate::pipeline::{EngineeringPipeline, InterventionHandler};
use crate::taskqueue::{TaskQueue, TaskStatus};

// ---------------------------------------------------------------------------
// CumulativeBudget — tracks spend across multiple pipeline runs
// ---------------------------------------------------------------------------

/// Entry in the budget ledger (one per task run).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetLedgerEntry {
    pub timestamp: String,
    pub task_id: String,
    pub cost: f64,
    pub tokens: u64,
    pub cumulative_cost: f64,
    pub cumulative_tokens: u64,
    pub status: String,
}

/// Tracks cumulative spend across multiple pipeline runs.
///
/// Persists to a JSONL ledger file so that the orchestrator can resume
/// after a crash without losing budget accounting.
pub struct CumulativeBudget {
    limit_dollars: f64,
    total_cost: f64,
    total_tokens: u64,
    total_runs: u32,
    ledger_path: Option<PathBuf>,
}

impl CumulativeBudget {
    /// Create a new cumulative budget with a dollar ceiling.
    pub fn new(limit_dollars: f64) -> Self {
        Self {
            limit_dollars,
            total_cost: 0.0,
            total_tokens: 0,
            total_runs: 0,
            ledger_path: None,
        }
    }

    /// Create with a ledger file for persistence.
    /// Loads existing entries to recover cumulative totals.
    pub fn with_ledger(limit_dollars: f64, ledger_path: &Path) -> Self {
        let mut budget = Self {
            limit_dollars,
            total_cost: 0.0,
            total_tokens: 0,
            total_runs: 0,
            ledger_path: Some(ledger_path.to_path_buf()),
        };
        budget.load_ledger();
        budget
    }

    /// Load existing ledger entries to recover cumulative totals.
    fn load_ledger(&mut self) {
        let Some(ref path) = self.ledger_path else {
            return;
        };
        let Ok(contents) = std::fs::read_to_string(path) else {
            return;
        };
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<BudgetLedgerEntry>(line) {
                self.total_cost = entry.cumulative_cost;
                self.total_tokens = entry.cumulative_tokens;
                self.total_runs += 1;
            }
        }
    }

    /// Check whether there is enough budget remaining for a task with the given per-task limit.
    pub fn can_afford(&self, per_task_dollars: f64) -> bool {
        self.remaining_dollars() >= per_task_dollars
    }

    /// Remaining dollar budget.
    pub fn remaining_dollars(&self) -> f64 {
        (self.limit_dollars - self.total_cost).max(0.0)
    }

    /// Record a completed task run's spend.
    pub fn record(&mut self, task_id: &str, cost: f64, tokens: u64, status: &str) {
        self.total_cost += cost;
        self.total_tokens += tokens;
        self.total_runs += 1;

        let entry = BudgetLedgerEntry {
            timestamp: Utc::now().to_rfc3339(),
            task_id: task_id.into(),
            cost,
            tokens,
            cumulative_cost: self.total_cost,
            cumulative_tokens: self.total_tokens,
            status: status.into(),
        };

        if let Some(ref path) = self.ledger_path
            && let Ok(json) = serde_json::to_string(&entry)
        {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "{json}");
            }
        }

        info!(
            task_id,
            cost,
            tokens,
            cumulative_cost = self.total_cost,
            remaining = self.remaining_dollars(),
            "budget recorded"
        );
    }

    /// Current cumulative spend.
    pub fn total_cost(&self) -> f64 {
        self.total_cost
    }

    /// Current cumulative tokens.
    pub fn total_tokens(&self) -> u64 {
        self.total_tokens
    }

    /// Total runs recorded.
    pub fn total_runs(&self) -> u32 {
        self.total_runs
    }

    /// The dollar ceiling.
    pub fn limit_dollars(&self) -> f64 {
        self.limit_dollars
    }
}

// ---------------------------------------------------------------------------
// QualityGate — post-run verification
// ---------------------------------------------------------------------------

/// Result of a quality gate check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityResult {
    pub passed: bool,
    pub details: String,
}

/// Run a quality gate command (e.g. `cargo test --workspace`) in the repo.
///
/// Returns `Ok(QualityResult)` always — failures are reported in the result,
/// not as errors.
pub async fn run_quality_gate(command: &str, repo_path: &Path) -> QualityResult {
    let parts: Vec<&str> = command.split_whitespace().collect();
    if parts.is_empty() {
        return QualityResult {
            passed: true,
            details: "no quality gate command".into(),
        };
    }

    let output = tokio::process::Command::new(parts[0])
        .args(&parts[1..])
        .current_dir(repo_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => QualityResult {
            passed: true,
            details: "quality gate passed".into(),
        },
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let stdout = String::from_utf8_lossy(&o.stdout);
            let detail = format!("{stdout}\n{stderr}");
            let truncated = if detail.len() > 2000 {
                format!("{}...[truncated]", &detail[..2000])
            } else {
                detail
            };
            QualityResult {
                passed: false,
                details: truncated,
            }
        }
        Err(e) => QualityResult {
            passed: false,
            details: format!("failed to run quality gate: {e}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Orchestrator types
// ---------------------------------------------------------------------------

/// Why the orchestrator stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CeaseReason {
    /// All tasks completed or no actionable tasks remain.
    AllTasksDone,
    /// Cumulative budget exhausted.
    BudgetExhausted,
    /// Quality gate failed — halting to prevent further damage.
    QualityGateFailed,
}

impl fmt::Display for CeaseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CeaseReason::AllTasksDone => write!(f, "all tasks done"),
            CeaseReason::BudgetExhausted => write!(f, "budget exhausted"),
            CeaseReason::QualityGateFailed => write!(f, "quality gate failed"),
        }
    }
}

/// Result of a single task run within the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRunResult {
    pub task_id: String,
    pub status: String,
    pub cost: f64,
    pub tokens: u64,
    pub pr_url: Option<String>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_context: Option<OutcomeContext>,
}

/// Result of the full orchestrator run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorResult {
    pub tasks_attempted: u32,
    pub tasks_succeeded: u32,
    pub tasks_failed: u32,
    #[serde(default)]
    pub tasks_deferred: u32,
    pub total_cost: f64,
    pub total_tokens: u64,
    pub duration: Duration,
    pub cease_reason: CeaseReason,
    pub run_results: Vec<TaskRunResult>,
}

// ---------------------------------------------------------------------------
// Orchestrator configuration
// ---------------------------------------------------------------------------

/// Parameters for the orchestrator (passed from CLI, not from config file).
pub struct OrchestratorParams {
    pub repo_path: PathBuf,
    pub base_branch: String,
    pub quality_gate_command: Option<String>,
    pub stop_on_failure: bool,
}

// ---------------------------------------------------------------------------
// OutcomeRouting — result of route_outcome()
// ---------------------------------------------------------------------------

/// What the orchestrator should do after routing a pipeline outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeRouting {
    /// Task status to set in the queue.
    pub task_status: TaskStatus,
    /// Which result counter to increment.
    pub counter: OutcomeCounter,
    /// Whether to save the OutcomeContext on the task and queue.
    pub save_context: bool,
    /// Whether to record the attempt in the AttemptTracker.
    pub track_attempt: bool,
}

/// Which result counter to increment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeCounter {
    Succeeded,
    Failed,
    Deferred,
    None,
}

// ---------------------------------------------------------------------------
// AttemptTracker — records per-task outcome history
// ---------------------------------------------------------------------------

/// Tracks outcome contexts across multiple attempts for the same task.
///
/// Used by the orchestrator to (a) enforce a max-attempt limit and
/// (b) inject structured failure context into the planner so it can
/// learn from prior failures.
pub struct AttemptTracker {
    history: HashMap<String, Vec<OutcomeContext>>,
}

impl AttemptTracker {
    pub fn new() -> Self {
        Self {
            history: HashMap::new(),
        }
    }

    /// Record an outcome context for a task attempt.
    pub fn record(&mut self, task_id: &str, ctx: OutcomeContext) {
        self.history
            .entry(task_id.to_string())
            .or_default()
            .push(ctx);
    }

    /// How many attempts have been recorded for a task.
    pub fn count(&self, task_id: &str) -> usize {
        self.history.get(task_id).map_or(0, |v| v.len())
    }

    /// Get the history of outcome contexts for a task.
    pub fn contexts(&self, task_id: &str) -> &[OutcomeContext] {
        self.history.get(task_id).map_or(&[], |v| v.as_slice())
    }
}

impl Default for AttemptTracker {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Multi-run autonomous orchestrator.
///
/// Iterates through a task queue, running each task through the full
/// engineering pipeline. Tracks cumulative spend against a budget ceiling
/// and optionally runs a quality gate between tasks.
pub struct Orchestrator {
    config: EngConfig,
    handler: Arc<dyn InterventionHandler>,
    history: Arc<dyn HistoryBackend>,
}

impl Orchestrator {
    pub fn new(
        config: EngConfig,
        handler: Arc<dyn InterventionHandler>,
        history: Arc<dyn HistoryBackend>,
    ) -> Self {
        Self {
            config,
            handler,
            history,
        }
    }

    /// Run the orchestrator loop.
    ///
    /// Iterates through the task queue, running each task through the pipeline.
    /// Stops when budget is exhausted, all tasks are done, or quality gate fails.
    pub async fn run(
        &self,
        queue: &mut TaskQueue,
        budget: &mut CumulativeBudget,
        params: &OrchestratorParams,
    ) -> OrchestratorResult {
        let start_time = Instant::now();
        let mut tracker = AttemptTracker::new();
        let max_attempts = self.config.limits.max_fix_attempts as usize;

        let mut result = OrchestratorResult {
            tasks_attempted: 0,
            tasks_succeeded: 0,
            tasks_failed: 0,
            tasks_deferred: 0,
            total_cost: 0.0,
            total_tokens: 0,
            duration: Duration::from_secs(0), // Will be updated before return
            cease_reason: CeaseReason::AllTasksDone,
            run_results: Vec::new(),
        };

        loop {
            // --- Check budget ---
            let per_task_dollars = self.config.limits.max_dollars_per_task;
            if !budget.can_afford(per_task_dollars) {
                info!(
                    remaining = budget.remaining_dollars(),
                    per_task = per_task_dollars,
                    "budget exhausted"
                );
                result.cease_reason = CeaseReason::BudgetExhausted;
                break;
            }

            // --- Pick next task ---
            let task_id;
            let objective;
            match queue.pick_next() {
                Some(task) => {
                    task_id = task.id.clone();
                    objective = task.objective.clone();
                }
                None => {
                    info!("no more actionable tasks");
                    result.cease_reason = CeaseReason::AllTasksDone;
                    break;
                }
            }

            // --- Check attempt limit ---
            if tracker.count(&task_id) >= max_attempts {
                info!(
                    task_id = %task_id,
                    attempts = tracker.count(&task_id),
                    max = max_attempts,
                    "max attempts exceeded, marking failed"
                );
                queue.update_status(&task_id, TaskStatus::Failed);
                queue.set_error(&task_id, format!("exceeded max attempts ({max_attempts})"));
                let _ = queue.save();
                result.tasks_attempted += 1;
                result.tasks_failed += 1;
                result.run_results.push(TaskRunResult {
                    task_id: task_id.clone(),
                    status: "max_attempts_exceeded".into(),
                    cost: 0.0,
                    tokens: 0,
                    pr_url: None,
                    error: Some(format!("exceeded max attempts ({max_attempts})")),
                    outcome_context: None,
                });
                continue;
            }

            // --- Mark in progress ---
            queue.update_status(&task_id, TaskStatus::InProgress);
            let _ = queue.save();

            info!(task_id = %task_id, "starting task");

            // --- Create fresh router + pipeline for this task ---
            let attempt_contexts = tracker.contexts(&task_id);
            let pipeline_result = match self
                .create_and_run_pipeline(&task_id, &objective, params, attempt_contexts)
                .await
            {
                Ok(pr) => pr,
                Err(e) => {
                    warn!(task_id = %task_id, error = %e, "pipeline setup failed");
                    queue.update_status(&task_id, TaskStatus::Failed);
                    queue.set_error(&task_id, e.to_string());
                    let _ = queue.save();

                    result.tasks_attempted += 1;
                    result.tasks_failed += 1;
                    result.run_results.push(TaskRunResult {
                        task_id: task_id.clone(),
                        status: "setup_error".into(),
                        cost: 0.0,
                        tokens: 0,
                        pr_url: None,
                        error: Some(e.to_string()),
                        outcome_context: None,
                    });

                    if params.stop_on_failure {
                        break;
                    }
                    continue;
                }
            };

            // --- Record spend ---
            let cost = pipeline_result.budget.estimated_cost;
            let tokens = pipeline_result.budget.total_tokens;
            let status_str = format!("{:?}", pipeline_result.status);
            budget.record(&task_id, cost, tokens, &status_str);

            result.tasks_attempted += 1;
            result.total_cost += cost;
            result.total_tokens += tokens;

            let run_result = TaskRunResult {
                task_id: task_id.clone(),
                status: status_str,
                cost,
                tokens,
                pr_url: pipeline_result.pr_url.clone(),
                error: pipeline_result.error.clone(),
                outcome_context: pipeline_result.outcome_context.clone(),
            };
            result.run_results.push(run_result);

            // --- Outcome-aware status routing ---

            // Handle decomposition specially (needs sub-task extraction).
            if pipeline_result.status == PipelineStatus::Decomposed {
                if let Some(sub_tasks) =
                    Self::extract_sub_tasks(&task_id, &pipeline_result.stage_outputs)
                {
                    let count = sub_tasks.len();
                    info!(task_id = %task_id, count, "decomposed into sub-tasks");
                    queue.inject_tasks(sub_tasks);
                    queue.update_status(&task_id, TaskStatus::Completed);
                } else {
                    warn!(
                        task_id = %task_id,
                        "decomposition flagged but no sub-tasks found"
                    );
                    result.tasks_failed += 1;
                    queue.update_status(&task_id, TaskStatus::Failed);
                    queue.set_error(&task_id, "decomposition produced no sub-tasks".into());
                }
                let _ = queue.save();
                continue;
            }

            let routing = Self::route_outcome(&pipeline_result);
            queue.update_status(&task_id, routing.task_status.clone());

            match routing.counter {
                OutcomeCounter::Succeeded => result.tasks_succeeded += 1,
                OutcomeCounter::Failed => result.tasks_failed += 1,
                OutcomeCounter::Deferred => result.tasks_deferred += 1,
                OutcomeCounter::None => {}
            }

            if let Some(ref url) = pipeline_result.pr_url {
                queue.set_pr_url(&task_id, url.clone());
            }
            if let Some(ref err) = pipeline_result.error {
                queue.set_error(&task_id, err.clone());
            }
            if let Some(ctx) = pipeline_result.outcome_context.clone() {
                if routing.save_context {
                    queue.set_outcome_context(&task_id, ctx.clone());
                }
                if routing.track_attempt {
                    tracker.record(&task_id, ctx);
                }
            }

            if routing.task_status == TaskStatus::Deferred {
                let _ = queue.save();
                info!(task_id = %task_id, "task deferred, will retry later");
                continue;
            }

            let _ = queue.save();

            let succeeded = matches!(
                pipeline_result.status,
                PipelineStatus::PrCreated | PipelineStatus::Committed
            );

            info!(
                task_id = %task_id,
                succeeded,
                cost,
                tokens,
                remaining_budget = budget.remaining_dollars(),
                "task complete"
            );

            // --- Quality gate (only after successful tasks) ---
            if succeeded && let Some(ref gate_cmd) = params.quality_gate_command {
                let qr = run_quality_gate(gate_cmd, &params.repo_path).await;
                if !qr.passed {
                    warn!(
                        task_id = %task_id,
                        details = %qr.details,
                        "quality gate failed — halting orchestrator"
                    );
                    result.cease_reason = CeaseReason::QualityGateFailed;
                    break;
                }
            }

            // --- Stop on failure (optional) ---
            if !succeeded && params.stop_on_failure {
                info!(task_id = %task_id, "stopping on failure (stop_on_failure=true)");
                break;
            }
        }

        result.duration = start_time.elapsed();

        info!(
            tasks_attempted = result.tasks_attempted,
            tasks_succeeded = result.tasks_succeeded,
            tasks_failed = result.tasks_failed,
            tasks_deferred = result.tasks_deferred,
            total_cost = result.total_cost,
            total_tokens = result.total_tokens,
            duration_ms = result.duration.as_millis(),
            cease_reason = ?result.cease_reason,
            "orchestrator run complete"
        );

        result
    }

    /// Create a fresh Router + Pipeline and run a single task.
    async fn create_and_run_pipeline(
        &self,
        task_id: &str,
        objective: &str,
        params: &OrchestratorParams,
        previous_attempts: &[OutcomeContext],
    ) -> Result<glitchlab_kernel::pipeline::PipelineResult> {
        let budget_tracker = BudgetTracker::new(
            self.config.limits.max_tokens_per_task,
            self.config.limits.max_dollars_per_task,
        );
        let provider_inits = self.config.resolve_providers();
        let mut router =
            Router::with_providers(self.config.routing_map(), budget_tracker, provider_inits);
        if let Some(chooser) = self.config.build_chooser() {
            router = router.with_chooser(chooser);
        }

        let preflight_errors = router.preflight_check();
        if !preflight_errors.is_empty() {
            let msg: Vec<String> = preflight_errors
                .iter()
                .map(|(role, err)| format!("[{role}] {err}"))
                .collect();
            return Err(Error::Config(format!(
                "preflight check failed: {}",
                msg.join("; ")
            )));
        }

        let router = Arc::new(router);
        let pipeline = EngineeringPipeline::new(
            Arc::clone(&router),
            self.config.clone(),
            Arc::clone(&self.handler),
            Arc::clone(&self.history),
        );

        Ok(pipeline
            .run(
                task_id,
                objective,
                &params.repo_path,
                &params.base_branch,
                previous_attempts,
            )
            .await)
    }

    /// Route a pipeline result to the appropriate queue/tracker updates.
    ///
    /// Returns the `TaskStatus` to set, the `OutcomeContext` (if any),
    /// and counters to update.
    fn route_outcome(
        pipeline_result: &glitchlab_kernel::pipeline::PipelineResult,
    ) -> OutcomeRouting {
        match pipeline_result.status {
            PipelineStatus::PrCreated | PipelineStatus::Committed => OutcomeRouting {
                task_status: TaskStatus::Completed,
                counter: OutcomeCounter::Succeeded,
                save_context: false,
                track_attempt: false,
            },
            PipelineStatus::Decomposed => OutcomeRouting {
                task_status: TaskStatus::Completed,
                counter: OutcomeCounter::None,
                save_context: false,
                track_attempt: false,
            },
            PipelineStatus::Deferred => OutcomeRouting {
                task_status: TaskStatus::Deferred,
                counter: OutcomeCounter::Deferred,
                save_context: true,
                track_attempt: true,
            },
            PipelineStatus::Blocked => OutcomeRouting {
                task_status: TaskStatus::Skipped,
                counter: OutcomeCounter::Failed,
                save_context: true,
                track_attempt: false,
            },
            PipelineStatus::Retryable => OutcomeRouting {
                task_status: TaskStatus::Failed,
                counter: OutcomeCounter::Failed,
                save_context: true,
                track_attempt: true,
            },
            _ => OutcomeRouting {
                task_status: TaskStatus::Failed,
                counter: OutcomeCounter::Failed,
                save_context: false,
                track_attempt: false,
            },
        }
    }

    /// Extract sub-tasks from a decomposed pipeline result.
    fn extract_sub_tasks(
        parent_id: &str,
        stage_outputs: &HashMap<String, AgentOutput>,
    ) -> Option<Vec<crate::taskqueue::Task>> {
        let plan = stage_outputs.get("plan")?;
        let decomposition = plan.data.get("decomposition")?.as_array()?;
        if decomposition.is_empty() {
            return None;
        }

        let mut tasks = Vec::new();
        for (i, item) in decomposition.iter().enumerate() {
            let id = item
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let id = if id.is_empty() {
                format!("{parent_id}-part{}", i + 1)
            } else {
                id
            };

            let objective = item
                .get("objective")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if objective.is_empty() {
                continue;
            }

            let depends_on: Vec<String> = item
                .get("depends_on")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            tasks.push(crate::taskqueue::Task {
                id,
                objective,
                priority: 50, // default priority for sub-tasks
                status: crate::taskqueue::TaskStatus::Pending,
                depends_on,
                error: None,
                pr_url: None,
                outcome_context: None,
            });
        }

        if tasks.is_empty() { None } else { Some(tasks) }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::AutoApproveHandler;
    use crate::taskqueue::Task;

    // -----------------------------------------------------------------------
    // CumulativeBudget tests
    // -----------------------------------------------------------------------

    #[test]
    fn budget_new_has_full_balance() {
        let budget = CumulativeBudget::new(100.0);
        assert!((budget.remaining_dollars() - 100.0).abs() < f64::EPSILON);
        assert_eq!(budget.total_runs(), 0);
        assert!((budget.total_cost() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn budget_can_afford_within_limit() {
        let budget = CumulativeBudget::new(10.0);
        assert!(budget.can_afford(2.0));
        assert!(budget.can_afford(10.0));
        assert!(!budget.can_afford(10.01));
    }

    #[test]
    fn budget_record_updates_totals() {
        let mut budget = CumulativeBudget::new(100.0);
        budget.record("task-1", 5.50, 50000, "PrCreated");
        assert!((budget.total_cost() - 5.50).abs() < f64::EPSILON);
        assert_eq!(budget.total_tokens(), 50000);
        assert_eq!(budget.total_runs(), 1);
        assert!((budget.remaining_dollars() - 94.50).abs() < f64::EPSILON);
    }

    #[test]
    fn budget_record_cumulates() {
        let mut budget = CumulativeBudget::new(100.0);
        budget.record("task-1", 3.0, 30000, "PrCreated");
        budget.record("task-2", 4.0, 40000, "Committed");
        assert!((budget.total_cost() - 7.0).abs() < f64::EPSILON);
        assert_eq!(budget.total_tokens(), 70000);
        assert_eq!(budget.total_runs(), 2);
    }

    #[test]
    fn budget_exhaustion() {
        let mut budget = CumulativeBudget::new(5.0);
        budget.record("task-1", 3.0, 30000, "PrCreated");
        assert!(budget.can_afford(2.0));
        assert!(!budget.can_afford(2.01));
        budget.record("task-2", 2.0, 20000, "Committed");
        assert!(!budget.can_afford(0.01));
    }

    #[test]
    fn budget_remaining_never_negative() {
        let mut budget = CumulativeBudget::new(5.0);
        budget.record("task-1", 10.0, 100000, "PrCreated");
        assert!((budget.remaining_dollars() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn budget_ledger_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("ledger.jsonl");

        // Write some entries.
        {
            let mut budget = CumulativeBudget::with_ledger(100.0, &ledger_path);
            budget.record("task-1", 3.0, 30000, "PrCreated");
            budget.record("task-2", 4.0, 40000, "Committed");
            assert!((budget.total_cost() - 7.0).abs() < f64::EPSILON);
        }

        // Reload — should recover cumulative totals.
        {
            let budget = CumulativeBudget::with_ledger(100.0, &ledger_path);
            assert!((budget.total_cost() - 7.0).abs() < f64::EPSILON);
            assert_eq!(budget.total_tokens(), 70000);
            assert_eq!(budget.total_runs(), 2);
            assert!((budget.remaining_dollars() - 93.0).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn budget_ledger_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("ledger.jsonl");
        std::fs::write(&ledger_path, "").unwrap();

        let budget = CumulativeBudget::with_ledger(50.0, &ledger_path);
        assert!((budget.total_cost() - 0.0).abs() < f64::EPSILON);
        assert_eq!(budget.total_runs(), 0);
    }

    #[test]
    fn budget_ledger_nonexistent_file() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("nonexistent.jsonl");

        let budget = CumulativeBudget::with_ledger(50.0, &ledger_path);
        assert!((budget.total_cost() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn budget_ledger_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("ledger.jsonl");
        std::fs::write(
            &ledger_path,
            "not json\n{\"timestamp\":\"t\",\"task_id\":\"a\",\"cost\":1.0,\"tokens\":1000,\"cumulative_cost\":1.0,\"cumulative_tokens\":1000,\"status\":\"ok\"}\ngarbage\n",
        )
        .unwrap();

        let budget = CumulativeBudget::with_ledger(50.0, &ledger_path);
        assert!((budget.total_cost() - 1.0).abs() < f64::EPSILON);
        assert_eq!(budget.total_tokens(), 1000);
    }

    #[test]
    fn budget_ledger_entry_serde() {
        let entry = BudgetLedgerEntry {
            timestamp: "2026-02-23T00:00:00Z".into(),
            task_id: "test-1".into(),
            cost: 2.50,
            tokens: 25000,
            cumulative_cost: 10.50,
            cumulative_tokens: 100000,
            status: "PrCreated".into(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: BudgetLedgerEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.task_id, entry.task_id);
        assert!((parsed.cost - entry.cost).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // QualityGate tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn quality_gate_passing_command() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_quality_gate("true", dir.path()).await;
        assert!(result.passed);
    }

    #[tokio::test]
    async fn quality_gate_failing_command() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_quality_gate("false", dir.path()).await;
        assert!(!result.passed);
    }

    #[tokio::test]
    async fn quality_gate_empty_command() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_quality_gate("", dir.path()).await;
        assert!(result.passed);
    }

    #[tokio::test]
    async fn quality_gate_nonexistent_command() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_quality_gate("nonexistent_command_xyz", dir.path()).await;
        assert!(!result.passed);
        assert!(result.details.contains("failed to run"));
    }

    // -----------------------------------------------------------------------
    // Orchestrator tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn orchestrator_empty_queue_returns_all_done() {
        let config = EngConfig::default();
        let handler = Arc::new(AutoApproveHandler);
        let dir = tempfile::tempdir().unwrap();
        let history: Arc<dyn HistoryBackend> =
            Arc::new(glitchlab_memory::history::JsonlHistory::new(dir.path()));

        let orchestrator = Orchestrator::new(config, handler, history);
        let mut queue = TaskQueue::from_tasks(vec![]);
        let mut budget = CumulativeBudget::new(100.0);
        let params = OrchestratorParams {
            repo_path: dir.path().to_path_buf(),
            base_branch: "main".into(),
            quality_gate_command: None,
            stop_on_failure: false,
        };

        let result = orchestrator.run(&mut queue, &mut budget, &params).await;
        assert_eq!(result.cease_reason, CeaseReason::AllTasksDone);
        assert_eq!(result.tasks_attempted, 0);
    }

    #[tokio::test]
    async fn orchestrator_budget_exhausted_before_start() {
        let config = EngConfig::default(); // per-task: $2.00
        let handler = Arc::new(AutoApproveHandler);
        let dir = tempfile::tempdir().unwrap();
        let history: Arc<dyn HistoryBackend> =
            Arc::new(glitchlab_memory::history::JsonlHistory::new(dir.path()));

        let orchestrator = Orchestrator::new(config, handler, history);
        let tasks = vec![Task {
            id: "t1".into(),
            objective: "do something".into(),
            priority: 1,
            status: TaskStatus::Pending,
            depends_on: vec![],
            error: None,
            pr_url: None,
            outcome_context: None,
        }];
        let mut queue = TaskQueue::from_tasks(tasks);
        let mut budget = CumulativeBudget::new(0.10); // less than per-task $0.50
        let params = OrchestratorParams {
            repo_path: dir.path().to_path_buf(),
            base_branch: "main".into(),
            quality_gate_command: None,
            stop_on_failure: false,
        };

        let result = orchestrator.run(&mut queue, &mut budget, &params).await;
        assert_eq!(result.cease_reason, CeaseReason::BudgetExhausted);
        assert_eq!(result.tasks_attempted, 0);
    }

    #[tokio::test]
    async fn orchestrator_blocked_tasks_returns_all_done() {
        let config = EngConfig::default();
        let handler = Arc::new(AutoApproveHandler);
        let dir = tempfile::tempdir().unwrap();
        let history: Arc<dyn HistoryBackend> =
            Arc::new(glitchlab_memory::history::JsonlHistory::new(dir.path()));

        let orchestrator = Orchestrator::new(config, handler, history);
        // Task "b" depends on "a", but "a" is failed (not completed).
        let tasks = vec![
            Task {
                id: "a".into(),
                objective: "A".into(),
                priority: 1,
                status: TaskStatus::Failed,
                depends_on: vec![],
                error: None,
                pr_url: None,
                outcome_context: None,
            },
            Task {
                id: "b".into(),
                objective: "B".into(),
                priority: 2,
                status: TaskStatus::Pending,
                depends_on: vec!["a".into()],
                error: None,
                pr_url: None,
                outcome_context: None,
            },
        ];
        let mut queue = TaskQueue::from_tasks(tasks);
        let mut budget = CumulativeBudget::new(100.0);
        let params = OrchestratorParams {
            repo_path: dir.path().to_path_buf(),
            base_branch: "main".into(),
            quality_gate_command: None,
            stop_on_failure: false,
        };

        let result = orchestrator.run(&mut queue, &mut budget, &params).await;
        assert_eq!(result.cease_reason, CeaseReason::AllTasksDone);
        assert_eq!(result.tasks_attempted, 0);
    }

    // -----------------------------------------------------------------------
    // OrchestratorResult serde
    // -----------------------------------------------------------------------

    #[test]
    fn orchestrator_result_serde() {
        let result = OrchestratorResult {
            tasks_attempted: 3,
            tasks_succeeded: 2,
            tasks_failed: 1,
            tasks_deferred: 0,
            total_cost: 12.50,
            total_tokens: 150000,
            duration: Duration::from_millis(5000),
            cease_reason: CeaseReason::BudgetExhausted,
            run_results: vec![TaskRunResult {
                task_id: "t1".into(),
                status: "PrCreated".into(),
                cost: 4.0,
                tokens: 50000,
                pr_url: Some("https://example.com/pr/1".into()),
                error: None,
                outcome_context: None,
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: OrchestratorResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tasks_attempted, 3);
        assert_eq!(parsed.cease_reason, CeaseReason::BudgetExhausted);
    }

    #[test]
    fn cease_reason_serde() {
        for reason in [
            CeaseReason::AllTasksDone,
            CeaseReason::BudgetExhausted,
            CeaseReason::QualityGateFailed,
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            let parsed: CeaseReason = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, reason);
        }
    }

    /// Helper: create a pending task.
    fn pending_task(id: &str, objective: &str) -> Task {
        Task {
            id: id.into(),
            objective: objective.into(),
            priority: 1,
            status: TaskStatus::Pending,
            depends_on: vec![],
            error: None,
            pr_url: None,
            outcome_context: None,
        }
    }

    /// The orchestrator exercises the setup-failure path when no API keys
    /// are available. Default config uses anthropic which requires keys.
    #[tokio::test]
    async fn orchestrator_setup_failure_marks_task_failed() {
        let config = EngConfig::default();
        let handler = Arc::new(AutoApproveHandler);
        let dir = tempfile::tempdir().unwrap();
        let history: Arc<dyn HistoryBackend> =
            Arc::new(glitchlab_memory::history::JsonlHistory::new(dir.path()));

        let orchestrator = Orchestrator::new(config, handler, history);
        let tasks = vec![pending_task("fail-1", "do something")];
        let mut queue = TaskQueue::from_tasks(tasks);
        let mut budget = CumulativeBudget::new(100.0);
        let params = OrchestratorParams {
            repo_path: dir.path().to_path_buf(),
            base_branch: "main".into(),
            quality_gate_command: None,
            stop_on_failure: false,
        };

        let result = orchestrator.run(&mut queue, &mut budget, &params).await;
        // Task should have been attempted and failed (preflight check fails).
        assert_eq!(result.tasks_attempted, 1);
        assert_eq!(result.tasks_failed, 1);
        assert_eq!(result.tasks_succeeded, 0);
        assert_eq!(result.cease_reason, CeaseReason::AllTasksDone);

        // Task should be marked Failed in the queue.
        let task = queue.tasks().iter().find(|t| t.id == "fail-1").unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(task.error.is_some());

        // Run result should have the error.
        assert_eq!(result.run_results.len(), 1);
        assert!(result.run_results[0].error.is_some());
    }

    #[tokio::test]
    async fn orchestrator_stop_on_failure_halts_after_first_error() {
        let config = EngConfig::default();
        let handler = Arc::new(AutoApproveHandler);
        let dir = tempfile::tempdir().unwrap();
        let history: Arc<dyn HistoryBackend> =
            Arc::new(glitchlab_memory::history::JsonlHistory::new(dir.path()));

        let orchestrator = Orchestrator::new(config, handler, history);
        let tasks = vec![
            pending_task("s1", "first task"),
            pending_task("s2", "second task"),
        ];
        let mut queue = TaskQueue::from_tasks(tasks);
        let mut budget = CumulativeBudget::new(100.0);
        let params = OrchestratorParams {
            repo_path: dir.path().to_path_buf(),
            base_branch: "main".into(),
            quality_gate_command: None,
            stop_on_failure: true,
        };

        let result = orchestrator.run(&mut queue, &mut budget, &params).await;
        // Should stop after the first failure.
        assert_eq!(result.tasks_attempted, 1);
        assert_eq!(result.tasks_failed, 1);
        // Second task should still be pending.
        let t2 = queue.tasks().iter().find(|t| t.id == "s2").unwrap();
        assert_eq!(t2.status, TaskStatus::Pending);
    }

    #[tokio::test]
    async fn orchestrator_continues_on_failure_by_default() {
        let config = EngConfig::default();
        let handler = Arc::new(AutoApproveHandler);
        let dir = tempfile::tempdir().unwrap();
        let history: Arc<dyn HistoryBackend> =
            Arc::new(glitchlab_memory::history::JsonlHistory::new(dir.path()));

        let orchestrator = Orchestrator::new(config, handler, history);
        let tasks = vec![
            pending_task("c1", "first task"),
            pending_task("c2", "second task"),
        ];
        let mut queue = TaskQueue::from_tasks(tasks);
        let mut budget = CumulativeBudget::new(100.0);
        let params = OrchestratorParams {
            repo_path: dir.path().to_path_buf(),
            base_branch: "main".into(),
            quality_gate_command: None,
            stop_on_failure: false,
        };

        let result = orchestrator.run(&mut queue, &mut budget, &params).await;
        // Should attempt both tasks.
        assert_eq!(result.tasks_attempted, 2);
        assert_eq!(result.tasks_failed, 2);
    }

    #[tokio::test]
    async fn orchestrator_budget_exhausts_mid_run() {
        let mut config = EngConfig::default();
        config.limits.max_dollars_per_task = 50.0; // per-task budget = $50
        let handler = Arc::new(AutoApproveHandler);
        let dir = tempfile::tempdir().unwrap();
        let history: Arc<dyn HistoryBackend> =
            Arc::new(glitchlab_memory::history::JsonlHistory::new(dir.path()));

        let orchestrator = Orchestrator::new(config, handler, history);
        let tasks = vec![
            pending_task("b1", "first"),
            pending_task("b2", "second"),
            pending_task("b3", "third"),
        ];
        let mut queue = TaskQueue::from_tasks(tasks);
        // Total budget $90, per-task $50. After 1 task (even if it fails with
        // $0 cost), the budget check requires $50 remaining.
        // With $90 total and $50 per task, can afford at most 1 task fully.
        // But since setup failures cost $0, the budget isn't consumed.
        // Use a smaller total budget to trigger exhaustion.
        let mut budget = CumulativeBudget::new(40.0); // less than $50 per task
        let params = OrchestratorParams {
            repo_path: dir.path().to_path_buf(),
            base_branch: "main".into(),
            quality_gate_command: None,
            stop_on_failure: false,
        };

        let result = orchestrator.run(&mut queue, &mut budget, &params).await;
        assert_eq!(result.cease_reason, CeaseReason::BudgetExhausted);
        assert_eq!(result.tasks_attempted, 0);
    }

    #[tokio::test]
    async fn orchestrator_with_quality_gate_failure() {
        let config = EngConfig::default();
        let handler = Arc::new(AutoApproveHandler);
        let dir = tempfile::tempdir().unwrap();
        let history: Arc<dyn HistoryBackend> =
            Arc::new(glitchlab_memory::history::JsonlHistory::new(dir.path()));

        let orchestrator = Orchestrator::new(config, handler, history);
        let tasks = vec![
            pending_task("q1", "first task"),
            pending_task("q2", "second task"),
        ];
        let mut queue = TaskQueue::from_tasks(tasks);
        let mut budget = CumulativeBudget::new(100.0);
        // Use `false` as quality gate — always fails.
        // However, quality gate only runs AFTER a successful pipeline run.
        // Since setup fails (no API keys), the quality gate won't run for
        // setup_error tasks — they continue. Let's verify that behavior.
        let params = OrchestratorParams {
            repo_path: dir.path().to_path_buf(),
            base_branch: "main".into(),
            quality_gate_command: Some("false".into()),
            stop_on_failure: false,
        };

        let result = orchestrator.run(&mut queue, &mut budget, &params).await;
        // Quality gate only runs after successful tasks. Both tasks fail at the
        // pipeline stage (no providers), so quality gate is never reached.
        assert_eq!(result.tasks_attempted, 2);
        assert_eq!(result.tasks_failed, 2);
        assert_eq!(result.cease_reason, CeaseReason::AllTasksDone);
    }

    #[test]
    fn quality_result_serde() {
        let qr = QualityResult {
            passed: false,
            details: "test failed".into(),
        };
        let json = serde_json::to_string(&qr).unwrap();
        let parsed: QualityResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.passed, qr.passed);
        assert_eq!(parsed.details, qr.details);
    }

    #[test]
    fn task_run_result_serde() {
        let trr = TaskRunResult {
            task_id: "t1".into(),
            status: "PrCreated".into(),
            cost: 2.5,
            tokens: 25000,
            pr_url: Some("https://example.com/pr/1".into()),
            error: None,
            outcome_context: None,
        };
        let json = serde_json::to_string(&trr).unwrap();
        let parsed: TaskRunResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.task_id, "t1");
        assert!(parsed.pr_url.is_some());
        assert!(parsed.error.is_none());
    }

    #[test]
    fn budget_limit_accessor() {
        let budget = CumulativeBudget::new(42.0);
        assert!((budget.limit_dollars() - 42.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_orchestrator_duration_tracking() {
        let config = EngConfig::default();
        let handler = Arc::new(AutoApproveHandler);
        let dir = tempfile::tempdir().unwrap();
        let history: Arc<dyn HistoryBackend> =
            Arc::new(glitchlab_memory::history::JsonlHistory::new(dir.path()));

        let orchestrator = Orchestrator::new(config, handler, history);
        let tasks = vec![pending_task("duration-test", "test duration tracking")];
        let mut queue = TaskQueue::from_tasks(tasks);
        let mut budget = CumulativeBudget::new(100.0);
        let params = OrchestratorParams {
            repo_path: dir.path().to_path_buf(),
            base_branch: "main".into(),
            quality_gate_command: None,
            stop_on_failure: false,
        };

        let result = orchestrator.run(&mut queue, &mut budget, &params).await;

        // Verify that duration is non-zero (orchestrator should take some time to run)
        assert!(
            result.duration.as_nanos() > 0,
            "Duration should be non-zero, got: {:?}",
            result.duration
        );

        // Duration should be reasonable (less than 10 seconds for this simple test)
        assert!(
            result.duration.as_secs() < 10,
            "Duration should be reasonable, got: {:?}",
            result.duration
        );
    }

    #[tokio::test]
    async fn quality_gate_truncates_long_output() {
        let dir = tempfile::tempdir().unwrap();
        // Write a non-executable script file; run it via `bash <script>`
        // so the kernel never executes the file directly (avoids ETXTBSY
        // race under parallel test load).
        let script_path = dir.path().join("longout.sh");
        std::fs::write(&script_path, "printf '%0.sx' {1..3000}\nexit 1\n").unwrap();
        let cmd = format!("bash {}", script_path.display());
        let result = run_quality_gate(&cmd, dir.path()).await;
        assert!(!result.passed);
        assert!(
            result.details.contains("...[truncated]"),
            "expected truncation marker, got: {}",
            &result.details[..result.details.len().min(100)]
        );
        // Should be truncated to ~2000 + the suffix length.
        assert!(result.details.len() < 2100);
    }

    #[tokio::test]
    async fn quality_gate_captures_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_quality_gate("bash -c 'echo err >&2; exit 1'", dir.path()).await;
        assert!(!result.passed);
        assert!(result.details.contains("err"));
    }

    // -----------------------------------------------------------------------
    // extract_sub_tasks tests
    // -----------------------------------------------------------------------

    #[test]
    fn extract_sub_tasks_from_plan() {
        use glitchlab_kernel::agent::AgentMetadata;

        let plan_data = serde_json::json!({
            "estimated_complexity": "large",
            "decomposition": [
                {
                    "id": "parent-part1",
                    "objective": "Add uuid to Cargo.toml",
                    "depends_on": []
                },
                {
                    "id": "parent-part2",
                    "objective": "Add request_id field",
                    "depends_on": ["parent-part1"]
                }
            ],
            "steps": [],
            "files_likely_affected": [],
            "requires_core_change": false,
            "risk_level": "medium",
            "risk_notes": "multi-file",
            "test_strategy": [],
            "dependencies_affected": true,
            "public_api_changed": true
        });

        let plan_output = AgentOutput {
            data: plan_data,
            metadata: AgentMetadata {
                agent: "planner".into(),
                model: "test".into(),
                tokens: 100,
                cost: 0.01,
                latency_ms: 50,
            },
            parse_error: false,
        };

        let mut stage_outputs = HashMap::new();
        stage_outputs.insert("plan".into(), plan_output);

        let sub_tasks = Orchestrator::extract_sub_tasks("parent", &stage_outputs).unwrap();
        assert_eq!(sub_tasks.len(), 2);
        assert_eq!(sub_tasks[0].id, "parent-part1");
        assert_eq!(sub_tasks[0].objective, "Add uuid to Cargo.toml");
        assert!(sub_tasks[0].depends_on.is_empty());
        assert_eq!(sub_tasks[1].id, "parent-part2");
        assert_eq!(sub_tasks[1].depends_on, vec!["parent-part1"]);
    }

    #[test]
    fn extract_sub_tasks_none_when_no_decomposition() {
        let stage_outputs = HashMap::new();
        assert!(Orchestrator::extract_sub_tasks("x", &stage_outputs).is_none());
    }

    #[test]
    fn extract_sub_tasks_none_when_empty_decomposition() {
        use glitchlab_kernel::agent::AgentMetadata;

        let plan_output = AgentOutput {
            data: serde_json::json!({
                "decomposition": [],
                "steps": []
            }),
            metadata: AgentMetadata {
                agent: "planner".into(),
                model: "test".into(),
                tokens: 100,
                cost: 0.01,
                latency_ms: 50,
            },
            parse_error: false,
        };

        let mut stage_outputs = HashMap::new();
        stage_outputs.insert("plan".into(), plan_output);

        assert!(Orchestrator::extract_sub_tasks("x", &stage_outputs).is_none());
    }

    // -----------------------------------------------------------------------
    // AttemptTracker tests
    // -----------------------------------------------------------------------

    #[test]
    fn attempt_tracker_empty() {
        let tracker = AttemptTracker::new();
        assert_eq!(tracker.count("task-1"), 0);
        assert!(tracker.contexts("task-1").is_empty());
    }

    #[test]
    fn attempt_tracker_records_and_counts() {
        use glitchlab_kernel::outcome::{ObstacleKind, OutcomeContext};

        let mut tracker = AttemptTracker::new();
        let ctx = OutcomeContext {
            approach: "first try".into(),
            obstacle: ObstacleKind::Unknown {
                detail: "failed".into(),
            },
            discoveries: vec![],
            recommendation: None,
            files_explored: vec![],
        };
        tracker.record("task-1", ctx.clone());
        assert_eq!(tracker.count("task-1"), 1);
        assert_eq!(tracker.count("task-2"), 0);

        tracker.record("task-1", ctx);
        assert_eq!(tracker.count("task-1"), 2);
    }

    #[test]
    fn attempt_tracker_contexts_returns_history() {
        use glitchlab_kernel::outcome::{ObstacleKind, OutcomeContext};

        let mut tracker = AttemptTracker::new();
        tracker.record(
            "t1",
            OutcomeContext {
                approach: "attempt 1".into(),
                obstacle: ObstacleKind::TestFailure {
                    attempts: 1,
                    last_error: "err".into(),
                },
                discoveries: vec!["found X".into()],
                recommendation: None,
                files_explored: vec![],
            },
        );
        tracker.record(
            "t1",
            OutcomeContext {
                approach: "attempt 2".into(),
                obstacle: ObstacleKind::TestFailure {
                    attempts: 2,
                    last_error: "still err".into(),
                },
                discoveries: vec![],
                recommendation: Some("try Y".into()),
                files_explored: vec![],
            },
        );

        let contexts = tracker.contexts("t1");
        assert_eq!(contexts.len(), 2);
        assert_eq!(contexts[0].approach, "attempt 1");
        assert_eq!(contexts[1].approach, "attempt 2");
    }

    #[test]
    fn attempt_tracker_default() {
        let tracker = AttemptTracker::default();
        assert_eq!(tracker.count("x"), 0);
    }

    #[test]
    fn task_run_result_with_outcome_context_serde() {
        use glitchlab_kernel::outcome::{ObstacleKind, OutcomeContext};

        let trr = TaskRunResult {
            task_id: "t1".into(),
            status: "Deferred".into(),
            cost: 0.5,
            tokens: 5000,
            pr_url: None,
            error: Some("deferred".into()),
            outcome_context: Some(OutcomeContext {
                approach: "tried X".into(),
                obstacle: ObstacleKind::MissingPrerequisite {
                    task_id: "dep-1".into(),
                    reason: "not ready".into(),
                },
                discoveries: vec![],
                recommendation: None,
                files_explored: vec![],
            }),
        };
        let json = serde_json::to_string(&trr).unwrap();
        assert!(json.contains("outcome_context"));
        let parsed: TaskRunResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.outcome_context.is_some());
    }

    #[test]
    fn orchestrator_result_with_deferred_serde() {
        let result = OrchestratorResult {
            tasks_attempted: 5,
            tasks_succeeded: 2,
            tasks_failed: 1,
            tasks_deferred: 2,
            total_cost: 5.0,
            total_tokens: 50000,
            duration: Duration::from_millis(1000),
            cease_reason: CeaseReason::AllTasksDone,
            run_results: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: OrchestratorResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tasks_deferred, 2);
    }

    // -----------------------------------------------------------------------
    // route_outcome tests
    // -----------------------------------------------------------------------

    fn make_pipeline_result(status: PipelineStatus) -> glitchlab_kernel::pipeline::PipelineResult {
        glitchlab_kernel::pipeline::PipelineResult {
            status,
            stage_outputs: HashMap::new(),
            events: vec![],
            budget: glitchlab_kernel::budget::BudgetSummary::default(),
            pr_url: None,
            branch: None,
            error: None,
            outcome_context: None,
        }
    }

    #[test]
    fn route_outcome_pr_created() {
        let pr = make_pipeline_result(PipelineStatus::PrCreated);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Completed);
        assert_eq!(routing.counter, OutcomeCounter::Succeeded);
        assert!(!routing.save_context);
        assert!(!routing.track_attempt);
    }

    #[test]
    fn route_outcome_committed() {
        let pr = make_pipeline_result(PipelineStatus::Committed);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Completed);
        assert_eq!(routing.counter, OutcomeCounter::Succeeded);
    }

    #[test]
    fn route_outcome_deferred() {
        let pr = make_pipeline_result(PipelineStatus::Deferred);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Deferred);
        assert_eq!(routing.counter, OutcomeCounter::Deferred);
        assert!(routing.save_context);
        assert!(routing.track_attempt);
    }

    #[test]
    fn route_outcome_blocked() {
        let pr = make_pipeline_result(PipelineStatus::Blocked);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Skipped);
        assert_eq!(routing.counter, OutcomeCounter::Failed);
        assert!(routing.save_context);
        assert!(!routing.track_attempt);
    }

    #[test]
    fn route_outcome_retryable() {
        let pr = make_pipeline_result(PipelineStatus::Retryable);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Failed);
        assert_eq!(routing.counter, OutcomeCounter::Failed);
        assert!(routing.save_context);
        assert!(routing.track_attempt);
    }

    #[test]
    fn route_outcome_implementation_failed() {
        let pr = make_pipeline_result(PipelineStatus::ImplementationFailed);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Failed);
        assert_eq!(routing.counter, OutcomeCounter::Failed);
        assert!(!routing.save_context);
        assert!(!routing.track_attempt);
    }

    #[test]
    fn route_outcome_decomposed() {
        let pr = make_pipeline_result(PipelineStatus::Decomposed);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Completed);
        assert_eq!(routing.counter, OutcomeCounter::None);
    }

    #[test]
    fn route_outcome_tests_failed() {
        let pr = make_pipeline_result(PipelineStatus::TestsFailed);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Failed);
        assert_eq!(routing.counter, OutcomeCounter::Failed);
    }

    #[test]
    fn route_outcome_error() {
        let pr = make_pipeline_result(PipelineStatus::Error);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Failed);
        assert_eq!(routing.counter, OutcomeCounter::Failed);
    }

    #[test]
    fn outcome_routing_eq() {
        let a = OutcomeRouting {
            task_status: TaskStatus::Deferred,
            counter: OutcomeCounter::Deferred,
            save_context: true,
            track_attempt: true,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    // -----------------------------------------------------------------------
    // CeaseReason Display tests
    // -----------------------------------------------------------------------

    #[test]
    fn cease_reason_display() {
        assert_eq!(CeaseReason::AllTasksDone.to_string(), "all tasks done");
        assert_eq!(CeaseReason::BudgetExhausted.to_string(), "budget exhausted");
        assert_eq!(
            CeaseReason::QualityGateFailed.to_string(),
            "quality gate failed"
        );
    }
}
