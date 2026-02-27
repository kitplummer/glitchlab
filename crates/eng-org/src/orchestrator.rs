use std::collections::{HashMap, VecDeque};
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
use crate::dashboard::{DashboardEmitter, DashboardEvent};
use crate::pipeline::{EngineeringPipeline, InterventionHandler, RealExternalOps};
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
    /// Cumulative cost of remediation tasks only.
    repair_cost: f64,
    /// Fraction of total budget reserved for repair (0.0–1.0).
    repair_budget_fraction: f64,
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
            repair_cost: 0.0,
            repair_budget_fraction: 0.20,
        }
    }

    /// Create with a ledger file for persistence.
    /// Each invocation starts with a fresh budget — the ledger is append-only
    /// for historical tracking, not for constraining the current run.
    pub fn with_ledger(limit_dollars: f64, ledger_path: &Path) -> Self {
        Self {
            limit_dollars,
            total_cost: 0.0,
            total_tokens: 0,
            total_runs: 0,
            ledger_path: Some(ledger_path.to_path_buf()),
            repair_cost: 0.0,
            repair_budget_fraction: 0.20,
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

    /// Set the repair budget fraction (builder-style).
    pub fn with_repair_budget_fraction(mut self, fraction: f64) -> Self {
        self.repair_budget_fraction = fraction.clamp(0.0, 1.0);
        self
    }

    /// Total dollars available for repair tasks.
    pub fn repair_budget_dollars(&self) -> f64 {
        self.limit_dollars * self.repair_budget_fraction
    }

    /// Remaining dollars available for repair tasks.
    pub fn repair_remaining(&self) -> f64 {
        (self.repair_budget_dollars() - self.repair_cost).max(0.0)
    }

    /// Check whether the repair budget can cover one more task at the given cost.
    pub fn can_afford_repair(&self, per_task_dollars: f64) -> bool {
        self.repair_remaining() >= per_task_dollars
    }

    /// Record a repair task's spend (tracks separately from feature spend).
    pub fn record_repair(&mut self, task_id: &str, cost: f64, tokens: u64, status: &str) {
        self.repair_cost += cost;
        self.record(task_id, cost, tokens, status);
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

/// Simple string hash for generating deterministic task IDs.
fn fxhash(s: &str) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish() as u32
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
    /// Too many failures in a short window — likely a systemic issue.
    SystemicFailure,
}

impl fmt::Display for CeaseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CeaseReason::AllTasksDone => write!(f, "all tasks done"),
            CeaseReason::BudgetExhausted => write!(f, "budget exhausted"),
            CeaseReason::QualityGateFailed => write!(f, "quality gate failed"),
            CeaseReason::SystemicFailure => write!(f, "systemic failure"),
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
    #[serde(default)]
    pub tasks_escalated: u32,
    pub total_cost: f64,
    pub total_tokens: u64,
    pub duration: Duration,
    pub cease_reason: CeaseReason,
    pub run_results: Vec<TaskRunResult>,
    /// The dominant failure pattern when systemic failure is detected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dominant_failure_pattern: Option<FailureCategory>,
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
    Escalated,
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
// FailureCategory — categorises failures for diagnostics
// ---------------------------------------------------------------------------

/// Categorises a pipeline failure for restart intensity diagnostics.
///
/// When `RestartIntensityMonitor` triggers, the dominant category tells the
/// operator *what* is failing systemically (e.g. all tests, or provider outage).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    SetupError,
    PlanFailed,
    ImplementationFailed,
    ParseError,
    TestsFailed,
    ArchitectRejected,
    MaxAttemptsExceeded,
    BudgetExceeded,
    Timeout,
    Other,
}

impl fmt::Display for FailureCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FailureCategory::SetupError => write!(f, "setup error"),
            FailureCategory::PlanFailed => write!(f, "plan failed"),
            FailureCategory::ImplementationFailed => write!(f, "implementation failed"),
            FailureCategory::ParseError => write!(f, "parse error"),
            FailureCategory::TestsFailed => write!(f, "tests failed"),
            FailureCategory::ArchitectRejected => write!(f, "architect rejected"),
            FailureCategory::MaxAttemptsExceeded => write!(f, "max attempts exceeded"),
            FailureCategory::BudgetExceeded => write!(f, "budget exceeded"),
            FailureCategory::Timeout => write!(f, "timeout"),
            FailureCategory::Other => write!(f, "other"),
        }
    }
}

/// Map a `PipelineStatus` to the appropriate `FailureCategory`.
pub fn pipeline_status_to_failure_category(status: PipelineStatus) -> FailureCategory {
    match status {
        PipelineStatus::PlanFailed => FailureCategory::PlanFailed,
        PipelineStatus::ImplementationFailed => FailureCategory::ImplementationFailed,
        PipelineStatus::ParseError => FailureCategory::ParseError,
        PipelineStatus::TestsFailed => FailureCategory::TestsFailed,
        PipelineStatus::ArchitectRejected => FailureCategory::ArchitectRejected,
        PipelineStatus::BudgetExceeded => FailureCategory::BudgetExceeded,
        PipelineStatus::TimedOut => FailureCategory::Timeout,
        _ => FailureCategory::Other,
    }
}

// ---------------------------------------------------------------------------
// RestartIntensityMonitor — sliding-window failure tracker
// ---------------------------------------------------------------------------

/// Tracks failure timestamps and categories in a sliding window. When the
/// number of failures within the window exceeds a threshold, signals that the
/// orchestrator should halt (likely a systemic issue such as a provider outage
/// or broken base branch). Returns the dominant failure category on trigger.
struct RestartIntensityMonitor {
    failures: VecDeque<(Instant, FailureCategory)>,
    max_failures: u32,
    window: Duration,
}

impl RestartIntensityMonitor {
    fn new(max_failures: u32, window_secs: u64) -> Self {
        Self {
            failures: VecDeque::new(),
            max_failures,
            window: Duration::from_secs(window_secs),
        }
    }

    /// Record a failure with its category.
    ///
    /// Returns `Some(dominant_category)` if the failure threshold is exceeded,
    /// where `dominant_category` is the most frequent category in the window.
    fn record_failure(&mut self, category: FailureCategory) -> Option<FailureCategory> {
        let now = Instant::now();
        self.failures.push_back((now, category));
        // Drain entries older than the window.
        while let Some(&(front_time, _)) = self.failures.front() {
            if now.duration_since(front_time) > self.window {
                self.failures.pop_front();
            } else {
                break;
            }
        }
        if self.failures.len() as u32 >= self.max_failures {
            Some(self.dominant_category())
        } else {
            None
        }
    }

    /// Find the most frequent category in the current window.
    fn dominant_category(&self) -> FailureCategory {
        let mut counts: HashMap<FailureCategory, u32> = HashMap::new();
        for &(_, cat) in &self.failures {
            *counts.entry(cat).or_insert(0) += 1;
        }
        counts
            .into_iter()
            .max_by_key(|&(_, count)| count)
            .map(|(cat, _)| cat)
            .unwrap_or(FailureCategory::Other)
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
    dashboard: Option<Arc<DashboardEmitter>>,
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
            dashboard: None,
        }
    }

    /// Attach a dashboard emitter for structured event output.
    pub fn with_dashboard(mut self, dashboard: Arc<DashboardEmitter>) -> Self {
        self.dashboard = Some(dashboard);
        self
    }

    /// Emit a dashboard event (no-op if no emitter attached).
    fn dash(&self, event: DashboardEvent) {
        if let Some(ref d) = self.dashboard {
            d.emit(event);
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
        let mut intensity_monitor = RestartIntensityMonitor::new(
            self.config.limits.restart_intensity_max_failures,
            self.config.limits.restart_intensity_window_secs,
        );
        let max_attempts = self.config.limits.max_fix_attempts as usize;

        let mut result = OrchestratorResult {
            tasks_attempted: 0,
            tasks_succeeded: 0,
            tasks_failed: 0,
            tasks_deferred: 0,
            tasks_escalated: 0,
            total_cost: 0.0,
            total_tokens: 0,
            duration: Duration::from_secs(0), // Will be updated before return
            cease_reason: CeaseReason::AllTasksDone,
            run_results: Vec::new(),
            dominant_failure_pattern: None,
        };

        let mut detected_patterns: Vec<crate::tqm::DetectedPattern> = vec![];

        self.dash(DashboardEvent::RunStarted {
            total_tasks: queue.summary().pending as usize,
            budget_dollars: budget.limit_dollars(),
        });

        // --- Task loop ---
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

            // --- Check max total tasks ---
            if queue.len() > self.config.limits.max_total_tasks as usize {
                warn!(
                    total = queue.len(),
                    max = self.config.limits.max_total_tasks,
                    "task queue exceeded max_total_tasks, halting"
                );
                result.cease_reason = CeaseReason::SystemicFailure;
                break;
            }

            // --- Pick next task ---
            let task_id;
            let objective;
            let task_depth;
            let task_is_remediation;
            match queue.pick_next() {
                Some(task) => {
                    task_id = task.id.clone();
                    // Append file hints from parent task to the objective so
                    // the planner/implementer can focus on specific files.
                    objective = if let Some(ref hints) = task.files_hint {
                        format!(
                            "{}\n\nFiles from parent task: {}",
                            task.objective,
                            hints.join(", ")
                        )
                    } else {
                        task.objective.clone()
                    };
                    task_depth = task.decomposition_depth;
                    task_is_remediation = task.is_remediation;
                    // Skip remediation tasks when repair budget is exhausted.
                    if task_is_remediation
                        && !budget.can_afford_repair(self.config.limits.max_dollars_per_task)
                    {
                        info!(task_id = %task_id, "skipping remediation task: repair budget exhausted");
                        queue.update_status(&task_id, crate::taskqueue::TaskStatus::Skipped);
                        continue;
                    }
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
                if let Some(dominant) =
                    intensity_monitor.record_failure(FailureCategory::MaxAttemptsExceeded)
                {
                    warn!(?dominant, "restart intensity exceeded");
                    result.cease_reason = CeaseReason::SystemicFailure;
                    result.dominant_failure_pattern = Some(dominant);
                    break;
                }
                continue;
            }

            // --- Mark in progress ---
            queue.update_status(&task_id, TaskStatus::InProgress);
            let _ = queue.save();

            info!(task_id = %task_id, "starting task");
            self.dash(DashboardEvent::TaskStarted {
                task_id: task_id.clone(),
                is_remediation: task_is_remediation,
            });

            // --- Create fresh router + pipeline for this task ---
            let attempt_contexts = tracker.contexts(&task_id);
            let pipeline_result = match self
                .create_and_run_pipeline(&task_id, &objective, params, attempt_contexts, task_depth)
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

                    if let Some(dominant) =
                        intensity_monitor.record_failure(FailureCategory::SetupError)
                    {
                        warn!(?dominant, "restart intensity exceeded");
                        result.cease_reason = CeaseReason::SystemicFailure;
                        result.dominant_failure_pattern = Some(dominant);
                        break;
                    }
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
            if task_is_remediation {
                budget.record_repair(&task_id, cost, tokens, &status_str);
            } else {
                budget.record(&task_id, cost, tokens, &status_str);
            }

            result.tasks_attempted += 1;
            result.total_cost += cost;
            result.total_tokens += tokens;

            // Dashboard: budget threshold check
            if let Some(ref d) = self.dashboard {
                d.check_budget_threshold(budget.total_cost(), budget.limit_dollars());
            }

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

            // --- Inline TQM: detect patterns mid-run ---
            if self.config.tqm.remediation_enabled {
                let new_patterns =
                    crate::tqm::analyze_incremental(&result, &self.config.tqm, &detected_patterns);
                for pattern in &new_patterns {
                    warn!(
                        kind = ?pattern.kind,
                        severity = %pattern.severity,
                        occurrences = pattern.occurrences,
                        "TQM: pattern detected mid-run"
                    );
                    self.dash(DashboardEvent::PatternDetected {
                        kind: format!("{:?}", pattern.kind),
                        severity: pattern.severity.clone(),
                        occurrences: pattern.occurrences,
                    });
                }
                if !new_patterns.is_empty() {
                    // Guard: skip Circuit if current task was already remediation (meta-loop)
                    // or if repair budget is exhausted.
                    let should_invoke_circuit = !task_is_remediation
                        && budget.can_afford_repair(self.config.limits.max_dollars_per_task);

                    if should_invoke_circuit {
                        // Try Circuit-powered remediation first.
                        let circuit_tasks = self
                            .invoke_circuit_for_patterns(&new_patterns, &result, budget)
                            .await;
                        if !circuit_tasks.is_empty() {
                            let ids: Vec<String> =
                                circuit_tasks.iter().map(|t| t.id.clone()).collect();
                            info!(
                                count = circuit_tasks.len(),
                                "TQM: Circuit injecting remediation tasks into queue"
                            );
                            self.dash(DashboardEvent::RemediationInjected {
                                source: "circuit".into(),
                                count: circuit_tasks.len(),
                                task_ids: ids,
                            });
                            queue.inject_tasks(circuit_tasks);
                        } else {
                            // Fallback to generic remediation if Circuit produced nothing.
                            let fallback_tasks =
                                crate::tqm::generate_deduplicated_remediation_tasks(
                                    &new_patterns,
                                    &self.config.tqm,
                                    queue,
                                );
                            if !fallback_tasks.is_empty() {
                                let ids: Vec<String> =
                                    fallback_tasks.iter().map(|t| t.id.clone()).collect();
                                info!(
                                    count = fallback_tasks.len(),
                                    "TQM: fallback injecting remediation tasks into queue"
                                );
                                self.dash(DashboardEvent::RemediationInjected {
                                    source: "tqm_fallback".into(),
                                    count: fallback_tasks.len(),
                                    task_ids: ids,
                                });
                                queue.inject_tasks(fallback_tasks);
                            }
                        }
                    } else {
                        // Guards failed — fall back to generic remediation.
                        let tasks = crate::tqm::generate_deduplicated_remediation_tasks(
                            &new_patterns,
                            &self.config.tqm,
                            queue,
                        );
                        if !tasks.is_empty() {
                            let ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
                            info!(
                                count = tasks.len(),
                                "TQM: injecting remediation tasks into queue"
                            );
                            self.dash(DashboardEvent::RemediationInjected {
                                source: "tqm_generic".into(),
                                count: tasks.len(),
                                task_ids: ids,
                            });
                            queue.inject_tasks(tasks);
                        }
                    }
                }
                detected_patterns.extend(new_patterns);
            }

            // --- Outcome-aware status routing ---

            // Handle decomposition specially (needs sub-task extraction).
            if pipeline_result.status == PipelineStatus::Decomposed {
                let max_depth = self.config.limits.max_decomposition_depth;
                let sub_ids = Self::handle_decomposition(
                    queue,
                    &mut result,
                    &task_id,
                    task_depth,
                    max_depth,
                    &pipeline_result.stage_outputs,
                );
                self.dash(DashboardEvent::TaskDecomposed {
                    task_id: task_id.clone(),
                    sub_task_ids: sub_ids,
                });
                continue;
            }

            // Dashboard: boundary violation
            if pipeline_result.status == PipelineStatus::BoundaryViolation {
                self.dash(DashboardEvent::BoundaryViolation {
                    task_id: task_id.clone(),
                });
            }

            let routing = Self::route_outcome(&pipeline_result);
            queue.update_status(&task_id, routing.task_status.clone());

            match routing.counter {
                OutcomeCounter::Succeeded => {
                    result.tasks_succeeded += 1;
                    self.dash(DashboardEvent::TaskSucceeded {
                        task_id: task_id.clone(),
                        status: format!("{:?}", pipeline_result.status),
                        cost,
                        tokens,
                        pr_url: pipeline_result.pr_url.clone(),
                    });
                    if let Some(ref url) = pipeline_result.pr_url {
                        self.dash(DashboardEvent::PrCreated {
                            task_id: task_id.clone(),
                            url: url.clone(),
                        });
                    }
                    if pipeline_result.status == PipelineStatus::PrMerged
                        && let Some(ref url) = pipeline_result.pr_url
                    {
                        self.dash(DashboardEvent::PrMerged {
                            task_id: task_id.clone(),
                            url: url.clone(),
                        });
                    }
                }
                OutcomeCounter::Failed => {
                    result.tasks_failed += 1;
                    self.dash(DashboardEvent::TaskFailed {
                        task_id: task_id.clone(),
                        status: format!("{:?}", pipeline_result.status),
                        cost,
                        reason: pipeline_result.error.clone(),
                    });
                    let category = pipeline_status_to_failure_category(pipeline_result.status);
                    if let Some(dominant) = intensity_monitor.record_failure(category) {
                        warn!(?dominant, "restart intensity exceeded");
                        result.cease_reason = CeaseReason::SystemicFailure;
                        result.dominant_failure_pattern = Some(dominant);
                        self.dash(DashboardEvent::SystemicFailure {
                            dominant_category: format!("{dominant:?}"),
                        });
                    }
                }
                OutcomeCounter::Deferred => result.tasks_deferred += 1,
                OutcomeCounter::Escalated => result.tasks_escalated += 1,
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

            if result.cease_reason == CeaseReason::SystemicFailure {
                break;
            }

            let succeeded = matches!(
                pipeline_result.status,
                PipelineStatus::PrCreated
                    | PipelineStatus::Committed
                    | PipelineStatus::PrMerged
                    | PipelineStatus::AlreadyDone
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
                    self.dash(DashboardEvent::QualityGateFailed {
                        details: qr.details,
                    });
                    result.cease_reason = CeaseReason::QualityGateFailed;
                    break;
                }
                self.dash(DashboardEvent::QualityGatePassed);
            }

            // --- Stop on failure (optional) ---
            if !succeeded && params.stop_on_failure {
                info!(task_id = %task_id, "stopping on failure (stop_on_failure=true)");
                break;
            }
        }

        result.duration = start_time.elapsed();

        // --- TQM end-of-run summary ---
        let tqm_patterns = crate::tqm::analyze(&result, &self.config.tqm);
        for pattern in &tqm_patterns {
            match pattern.severity.as_str() {
                "critical" => warn!(
                    kind = ?pattern.kind,
                    occurrences = pattern.occurrences,
                    affected = ?pattern.affected_tasks,
                    "{}", pattern.description
                ),
                "warning" => warn!(
                    kind = ?pattern.kind,
                    occurrences = pattern.occurrences,
                    "{}", pattern.description
                ),
                _ => info!(
                    kind = ?pattern.kind,
                    occurrences = pattern.occurrences,
                    "{}", pattern.description
                ),
            }
        }

        info!(
            tasks_attempted = result.tasks_attempted,
            tasks_succeeded = result.tasks_succeeded,
            tasks_failed = result.tasks_failed,
            tasks_deferred = result.tasks_deferred,
            tasks_escalated = result.tasks_escalated,
            total_cost = result.total_cost,
            total_tokens = result.total_tokens,
            duration_ms = result.duration.as_millis(),
            cease_reason = ?result.cease_reason,
            "orchestrator run complete"
        );

        self.dash(DashboardEvent::RunCompleted {
            succeeded: result.tasks_succeeded,
            failed: result.tasks_failed,
            deferred: result.tasks_deferred,
            escalated: result.tasks_escalated,
            total_cost: result.total_cost,
            duration_secs: result.duration.as_secs_f64(),
            cease_reason: format!("{:?}", result.cease_reason),
        });

        result
    }

    /// Create a fresh Router + Pipeline and run a single task.
    async fn create_and_run_pipeline(
        &self,
        task_id: &str,
        objective: &str,
        params: &OrchestratorParams,
        previous_attempts: &[OutcomeContext],
        decomposition_depth: u32,
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
            Arc::new(RealExternalOps),
        );

        Ok(pipeline
            .run_with_depth(
                task_id,
                objective,
                &params.repo_path,
                &params.base_branch,
                previous_attempts,
                decomposition_depth,
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
            PipelineStatus::PrCreated
            | PipelineStatus::Committed
            | PipelineStatus::PrMerged
            | PipelineStatus::AlreadyDone => OutcomeRouting {
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
                task_status: TaskStatus::Deferred,
                counter: OutcomeCounter::Deferred,
                save_context: true,
                track_attempt: true,
            },
            PipelineStatus::ArchitectRejected => OutcomeRouting {
                task_status: TaskStatus::Deferred,
                counter: OutcomeCounter::Deferred,
                save_context: true,
                track_attempt: true,
            },
            PipelineStatus::ParseError => OutcomeRouting {
                task_status: TaskStatus::Deferred,
                counter: OutcomeCounter::Deferred,
                save_context: true,
                track_attempt: true,
            },
            PipelineStatus::Escalated => OutcomeRouting {
                task_status: TaskStatus::Skipped,
                counter: OutcomeCounter::Escalated,
                save_context: true,
                track_attempt: false,
            },
            PipelineStatus::ImplementationFailed
            | PipelineStatus::TestsFailed
            | PipelineStatus::BoundaryViolation => OutcomeRouting {
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

    /// Invoke the Circuit (OpsDiagnosisAgent) for detected TQM patterns.
    ///
    /// Returns remediation tasks extracted from Circuit's output. If Circuit
    /// fails or escalates, returns an empty vec (caller should fall back).
    async fn invoke_circuit_for_patterns(
        &self,
        patterns: &[crate::tqm::DetectedPattern],
        run_result: &OrchestratorResult,
        budget: &CumulativeBudget,
    ) -> Vec<crate::taskqueue::Task> {
        use crate::agents::ops::OpsDiagnosisAgent;
        use glitchlab_kernel::agent::{Agent, AgentContext};

        // Build a lightweight router for the Circuit agent.
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
        let router = Arc::new(router);

        let patterns_json = serde_json::to_string(patterns).unwrap_or_default();
        let budget_summary = serde_json::json!({
            "total_cost": budget.total_cost(),
            "remaining": budget.remaining_dollars(),
            "repair_remaining": budget.repair_remaining(),
            "tasks_attempted": run_result.tasks_attempted,
            "tasks_failed": run_result.tasks_failed,
        });

        let mut extra = std::collections::HashMap::new();
        extra.insert("budget_summary".into(), budget_summary);

        let objective = format!(
            "Diagnose the following TQM-detected patterns and produce specific remediation tasks.\n\n\
             ## Detected Patterns\n\n{patterns_json}\n\n\
             ## Run Summary\n\n\
             Tasks attempted: {}, succeeded: {}, failed: {}, deferred: {}\n\
             Total cost: ${:.2}, remaining: ${:.2}",
            run_result.tasks_attempted,
            run_result.tasks_succeeded,
            run_result.tasks_failed,
            run_result.tasks_deferred,
            budget.total_cost(),
            budget.remaining_dollars(),
        );

        let ctx = AgentContext {
            task_id: "circuit-diagnosis".into(),
            objective,
            repo_path: String::new(),
            working_dir: String::new(),
            constraints: vec![
                "Never suggest changes to crates/kernel".into(),
                "Each remediation task should touch at most 2 files".into(),
            ],
            acceptance_criteria: vec![],
            risk_level: "low".into(),
            file_context: std::collections::HashMap::new(),
            previous_output: serde_json::Value::Null,
            extra,
        };

        let agent = OpsDiagnosisAgent::new(router);
        let output = match agent.execute(&ctx).await {
            Ok(out) => out,
            Err(e) => {
                warn!(error = %e, "Circuit agent failed, falling back to generic remediation");
                return Vec::new();
            }
        };

        // Parse Circuit output and convert to tasks.
        Self::circuit_output_to_tasks(
            &output.data,
            &self.config.tqm,
            self.config.limits.max_remediation_depth,
        )
    }

    /// Convert Circuit agent output JSON into remediation tasks.
    ///
    /// Filters by scope (`within_scope: true`) and escalation (`escalate: false`).
    fn circuit_output_to_tasks(
        output: &serde_json::Value,
        tqm_config: &crate::tqm::TQMConfig,
        max_remediation_depth: u32,
    ) -> Vec<crate::taskqueue::Task> {
        // If escalation is requested or scope is out of bounds, skip.
        if output
            .get("escalate")
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
        {
            info!("Circuit escalated — skipping task generation");
            return Vec::new();
        }
        if let Some(scope) = output.get("scope_assessment")
            && !scope
                .get("within_scope")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        {
            info!("Circuit: out of scope — skipping task generation");
            return Vec::new();
        }

        let priority = output
            .get("suggested_priority")
            .and_then(|v| v.as_u64())
            .unwrap_or(tqm_config.remediation_priority as u64) as u32;

        let remediation_tasks = match output.get("remediation_tasks").and_then(|v| v.as_array()) {
            Some(tasks) => tasks,
            None => return Vec::new(),
        };

        if max_remediation_depth == 0 {
            return Vec::new();
        }

        remediation_tasks
            .iter()
            .enumerate()
            .filter_map(|(i, task_val)| {
                let objective = task_val.get("objective")?.as_str()?;
                let id = format!("circuit-fix-{i}-{:08x}", fxhash(objective));
                Some(crate::taskqueue::Task {
                    id,
                    objective: objective.into(),
                    priority,
                    status: crate::taskqueue::TaskStatus::Pending,
                    depends_on: vec![],
                    decomposition_depth: 0,
                    error: None,
                    pr_url: None,
                    outcome_context: None,
                    remediation_depth: 1,
                    is_remediation: true,
                    files_hint: None,
                })
            })
            .collect()
    }

    /// Handle a decomposed pipeline result: check depth, extract sub-tasks, update queue.
    ///
    /// Returns `true` if decomposition succeeded (sub-tasks injected),
    /// `false` if it failed (depth exceeded or no sub-tasks found).
    /// Returns the IDs of injected sub-tasks (empty if decomposition failed).
    fn handle_decomposition(
        queue: &mut TaskQueue,
        result: &mut OrchestratorResult,
        task_id: &str,
        task_depth: u32,
        max_depth: u32,
        stage_outputs: &HashMap<String, AgentOutput>,
    ) -> Vec<String> {
        if task_depth >= max_depth {
            warn!(
                task_id = %task_id,
                depth = task_depth,
                max = max_depth,
                "decomposition depth exceeded, marking failed"
            );
            queue.update_status(task_id, TaskStatus::Failed);
            queue.set_error(
                task_id,
                format!("decomposition depth {task_depth} exceeds max {max_depth}"),
            );
            result.tasks_failed += 1;
            let _ = queue.save();
            return Vec::new();
        }

        if let Some(sub_tasks) = Self::extract_sub_tasks(task_id, task_depth, stage_outputs) {
            let count = sub_tasks.len();
            let ids: Vec<String> = sub_tasks.iter().map(|t| t.id.clone()).collect();
            info!(task_id = %task_id, count, "decomposed into sub-tasks");
            queue.inject_tasks(sub_tasks);
            queue.update_status(task_id, TaskStatus::Completed);
            let _ = queue.save();
            ids
        } else {
            warn!(
                task_id = %task_id,
                "decomposition flagged but no sub-tasks found"
            );
            result.tasks_failed += 1;
            queue.update_status(task_id, TaskStatus::Failed);
            queue.set_error(task_id, "decomposition produced no sub-tasks".into());
            let _ = queue.save();
            Vec::new()
        }
    }

    /// Extract sub-tasks from a decomposed pipeline result.
    fn extract_sub_tasks(
        parent_id: &str,
        parent_depth: u32,
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

            let files_hint: Option<Vec<String>> = item
                .get("files_likely_affected")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });

            tasks.push(crate::taskqueue::Task {
                id,
                objective,
                priority: 50, // default priority for sub-tasks
                status: crate::taskqueue::TaskStatus::Pending,
                depends_on,
                decomposition_depth: parent_depth + 1,
                error: None,
                pr_url: None,
                outcome_context: None,
                remediation_depth: 0,
                is_remediation: false,
                files_hint,
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

        // New session starts fresh — ledger is append-only for history,
        // not for constraining the current run's budget.
        {
            let budget = CumulativeBudget::with_ledger(100.0, &ledger_path);
            assert!((budget.total_cost() - 0.0).abs() < f64::EPSILON);
            assert_eq!(budget.total_tokens(), 0);
            assert_eq!(budget.total_runs(), 0);
            assert!((budget.remaining_dollars() - 100.0).abs() < f64::EPSILON);
        }

        // Ledger file should still contain the entries from the first session.
        let contents = std::fs::read_to_string(&ledger_path).unwrap();
        assert_eq!(contents.lines().count(), 2);
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

        // Each session starts fresh — pre-existing ledger entries are
        // historical records and don't affect the current budget.
        let budget = CumulativeBudget::with_ledger(50.0, &ledger_path);
        assert!((budget.total_cost() - 0.0).abs() < f64::EPSILON);
        assert_eq!(budget.total_tokens(), 0);
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
    // Repair budget tests
    // -----------------------------------------------------------------------

    #[test]
    fn repair_budget_defaults() {
        let budget = CumulativeBudget::new(100.0);
        assert!((budget.repair_budget_dollars() - 20.0).abs() < f64::EPSILON);
        assert!((budget.repair_remaining() - 20.0).abs() < f64::EPSILON);
        assert!(budget.can_afford_repair(10.0));
    }

    #[test]
    fn can_afford_repair() {
        let budget = CumulativeBudget::new(100.0).with_repair_budget_fraction(0.10);
        // 100 * 0.10 = $10 repair budget
        assert!((budget.repair_budget_dollars() - 10.0).abs() < f64::EPSILON);
        assert!(budget.can_afford_repair(10.0));
        assert!(!budget.can_afford_repair(10.01));
    }

    #[test]
    fn record_repair_tracks_separately() {
        let mut budget = CumulativeBudget::new(100.0).with_repair_budget_fraction(0.20);
        budget.record_repair("fix-1", 5.0, 1000, "Completed");
        // Total cost should reflect the repair.
        assert!((budget.total_cost() - 5.0).abs() < f64::EPSILON);
        // Repair remaining should decrease.
        assert!((budget.repair_remaining() - 15.0).abs() < f64::EPSILON);
        // Another repair of $16 should not be affordable.
        assert!(!budget.can_afford_repair(16.0));
        // But $15 exactly is.
        assert!(budget.can_afford_repair(15.0));
    }

    #[test]
    fn repair_budget_fraction_clamped() {
        let budget = CumulativeBudget::new(100.0).with_repair_budget_fraction(1.5);
        assert!((budget.repair_budget_dollars() - 100.0).abs() < f64::EPSILON);

        let budget = CumulativeBudget::new(100.0).with_repair_budget_fraction(-0.5);
        assert!((budget.repair_budget_dollars() - 0.0).abs() < f64::EPSILON);
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
            decomposition_depth: 0,
            error: None,
            pr_url: None,
            outcome_context: None,
            remediation_depth: 0,
            is_remediation: false,
            files_hint: None,
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
                decomposition_depth: 0,
                error: None,
                pr_url: None,
                outcome_context: None,
                remediation_depth: 0,
                is_remediation: false,
                files_hint: None,
            },
            Task {
                id: "b".into(),
                objective: "B".into(),
                priority: 2,
                status: TaskStatus::Pending,
                depends_on: vec!["a".into()],
                decomposition_depth: 0,
                error: None,
                pr_url: None,
                outcome_context: None,
                remediation_depth: 0,
                is_remediation: false,
                files_hint: None,
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
            tasks_escalated: 0,
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
            dominant_failure_pattern: None,
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
            CeaseReason::SystemicFailure,
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
            decomposition_depth: 0,
            error: None,
            pr_url: None,
            outcome_context: None,
            remediation_depth: 0,
            is_remediation: false,
            files_hint: None,
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

        let sub_tasks = Orchestrator::extract_sub_tasks("parent", 0, &stage_outputs).unwrap();
        assert_eq!(sub_tasks.len(), 2);
        assert_eq!(sub_tasks[0].id, "parent-part1");
        assert_eq!(sub_tasks[0].objective, "Add uuid to Cargo.toml");
        assert!(sub_tasks[0].depends_on.is_empty());
        assert_eq!(sub_tasks[1].id, "parent-part2");
        assert_eq!(sub_tasks[1].depends_on, vec!["parent-part1"]);
    }

    #[test]
    fn extract_sub_tasks_with_files_hint() {
        use glitchlab_kernel::agent::AgentMetadata;

        let plan_data = serde_json::json!({
            "estimated_complexity": "large",
            "decomposition": [
                {
                    "id": "parent-part1",
                    "objective": "Refactor module A",
                    "files_likely_affected": ["src/a.rs", "src/a_test.rs"],
                    "depends_on": []
                },
                {
                    "id": "parent-part2",
                    "objective": "Refactor module B",
                    "depends_on": ["parent-part1"]
                }
            ],
            "steps": [],
            "files_likely_affected": ["src/a.rs", "src/a_test.rs", "src/b.rs"],
            "requires_core_change": false,
            "risk_level": "medium",
            "risk_notes": "multi-file",
            "test_strategy": [],
            "dependencies_affected": false,
            "public_api_changed": false
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

        let sub_tasks = Orchestrator::extract_sub_tasks("parent", 0, &stage_outputs).unwrap();
        assert_eq!(sub_tasks.len(), 2);

        // First sub-task has files_hint from planner's files_likely_affected.
        assert_eq!(
            sub_tasks[0].files_hint,
            Some(vec!["src/a.rs".into(), "src/a_test.rs".into()])
        );
        // Second sub-task has no files_likely_affected → files_hint is None.
        assert!(sub_tasks[1].files_hint.is_none());
    }

    #[test]
    fn extract_sub_tasks_none_when_no_decomposition() {
        let stage_outputs = HashMap::new();
        assert!(Orchestrator::extract_sub_tasks("x", 0, &stage_outputs).is_none());
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

        assert!(Orchestrator::extract_sub_tasks("x", 0, &stage_outputs).is_none());
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

    use glitchlab_kernel::outcome::{ObstacleKind, OutcomeContext};

    fn make_outcome_context(approach: &str) -> OutcomeContext {
        OutcomeContext {
            approach: approach.into(),
            obstacle: ObstacleKind::Unknown {
                detail: "test".into(),
            },
            discoveries: vec![],
            recommendation: None,
            files_explored: vec![],
        }
    }

    #[test]
    fn test_count_unknown_task_id() {
        let tracker = AttemptTracker::new();
        assert_eq!(tracker.count("non-existent-task"), 0);
    }

    #[test]
    fn test_record_increments_count() {
        let mut tracker = AttemptTracker::new();
        let task_id = "task-1";

        assert_eq!(tracker.count(task_id), 0);

        tracker.record(task_id, make_outcome_context("attempt1"));
        assert_eq!(tracker.count(task_id), 1);

        tracker.record(task_id, make_outcome_context("attempt2"));
        assert_eq!(tracker.count(task_id), 2);
    }

    #[test]
    fn test_contexts_returns_in_order() {
        let mut tracker = AttemptTracker::new();
        let task_id = "task-1";

        let ctx1 = make_outcome_context("first");
        let ctx2 = make_outcome_context("second");
        let ctx3 = make_outcome_context("third");

        tracker.record(task_id, ctx1.clone());
        tracker.record(task_id, ctx2.clone());
        tracker.record(task_id, ctx3.clone());

        let contexts = tracker.contexts(task_id);
        assert_eq!(contexts.len(), 3);
        assert_eq!(contexts[0].approach, "first");
        assert_eq!(contexts[1].approach, "second");
        assert_eq!(contexts[2].approach, "third");
    }

    #[test]
    fn test_multiple_task_ids_tracked_independently() {
        let mut tracker = AttemptTracker::new();

        let ctx1_1 = make_outcome_context("task1-attempt1");
        let ctx1_2 = make_outcome_context("task1-attempt2");
        let ctx2_1 = make_outcome_context("task2-attempt1");

        tracker.record("task-1", ctx1_1);
        tracker.record("task-2", ctx2_1);
        tracker.record("task-1", ctx1_2);

        assert_eq!(tracker.count("task-1"), 2);
        assert_eq!(tracker.count("task-2"), 1);
        assert_eq!(tracker.count("task-3"), 0);

        let contexts1 = tracker.contexts("task-1");
        assert_eq!(contexts1.len(), 2);
        assert_eq!(contexts1[0].approach, "task1-attempt1");
        assert_eq!(contexts1[1].approach, "task1-attempt2");

        let contexts2 = tracker.contexts("task-2");
        assert_eq!(contexts2.len(), 1);
        assert_eq!(contexts2[0].approach, "task2-attempt1");
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
            tasks_escalated: 0,
            total_cost: 5.0,
            total_tokens: 50000,
            duration: Duration::from_millis(1000),
            cease_reason: CeaseReason::AllTasksDone,
            run_results: vec![],
            dominant_failure_pattern: None,
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
        assert_eq!(routing.task_status, TaskStatus::Deferred);
        assert_eq!(routing.counter, OutcomeCounter::Deferred);
        assert!(routing.save_context);
        assert!(routing.track_attempt);
    }

    #[test]
    fn route_outcome_implementation_failed() {
        let pr = make_pipeline_result(PipelineStatus::ImplementationFailed);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Failed);
        assert_eq!(routing.counter, OutcomeCounter::Failed);
        assert!(routing.save_context);
        assert!(routing.track_attempt);
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
        assert!(routing.save_context);
        assert!(routing.track_attempt);
    }

    #[test]
    fn route_outcome_boundary_violation_tracks_context() {
        let pr = make_pipeline_result(PipelineStatus::BoundaryViolation);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Failed);
        assert_eq!(routing.counter, OutcomeCounter::Failed);
        assert!(routing.save_context);
        assert!(routing.track_attempt);
    }

    #[test]
    fn route_outcome_error() {
        let pr = make_pipeline_result(PipelineStatus::Error);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Failed);
        assert_eq!(routing.counter, OutcomeCounter::Failed);
    }

    #[test]
    fn route_outcome_already_done() {
        let pr = make_pipeline_result(PipelineStatus::AlreadyDone);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Completed);
        assert_eq!(routing.counter, OutcomeCounter::Succeeded);
        assert!(!routing.save_context);
        assert!(!routing.track_attempt);
    }

    #[test]
    fn route_outcome_architect_rejected() {
        let pr = make_pipeline_result(PipelineStatus::ArchitectRejected);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Deferred);
        assert_eq!(routing.counter, OutcomeCounter::Deferred);
        assert!(routing.save_context);
        assert!(routing.track_attempt);
    }

    #[test]
    fn route_outcome_pr_merged() {
        let pr = make_pipeline_result(PipelineStatus::PrMerged);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Completed);
        assert_eq!(routing.counter, OutcomeCounter::Succeeded);
        assert!(!routing.save_context);
        assert!(!routing.track_attempt);
    }

    #[test]
    fn route_outcome_parse_error() {
        let pr = make_pipeline_result(PipelineStatus::ParseError);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Deferred);
        assert_eq!(routing.counter, OutcomeCounter::Deferred);
        assert!(routing.save_context);
        assert!(routing.track_attempt);
    }

    #[test]
    fn route_outcome_escalated() {
        let pr = make_pipeline_result(PipelineStatus::Escalated);
        let routing = Orchestrator::route_outcome(&pr);
        assert_eq!(routing.task_status, TaskStatus::Skipped);
        assert_eq!(routing.counter, OutcomeCounter::Escalated);
        assert!(routing.save_context);
        assert!(!routing.track_attempt);
    }

    #[test]
    fn orchestrator_result_escalated_serde() {
        let result = OrchestratorResult {
            tasks_attempted: 4,
            tasks_succeeded: 1,
            tasks_failed: 1,
            tasks_deferred: 1,
            tasks_escalated: 1,
            total_cost: 8.0,
            total_tokens: 80000,
            duration: Duration::from_millis(2000),
            cease_reason: CeaseReason::AllTasksDone,
            run_results: vec![],
            dominant_failure_pattern: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: OrchestratorResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tasks_escalated, 1);
    }

    #[test]
    fn orchestrator_result_backward_compat_without_escalated() {
        // Simulate deserializing old JSON without tasks_escalated field.
        let json = r#"{
            "tasks_attempted": 2,
            "tasks_succeeded": 1,
            "tasks_failed": 1,
            "total_cost": 3.0,
            "total_tokens": 30000,
            "duration": {"secs": 1, "nanos": 0},
            "cease_reason": "all_tasks_done",
            "run_results": []
        }"#;
        let parsed: OrchestratorResult = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.tasks_deferred, 0);
        assert_eq!(parsed.tasks_escalated, 0);
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

    // -----------------------------------------------------------------------
    // FailureCategory Display tests
    // -----------------------------------------------------------------------

    #[test]
    fn failure_category_display() {
        assert_eq!(FailureCategory::SetupError.to_string(), "setup error");
        assert_eq!(FailureCategory::PlanFailed.to_string(), "plan failed");
        assert_eq!(
            FailureCategory::ImplementationFailed.to_string(),
            "implementation failed"
        );
        assert_eq!(FailureCategory::TestsFailed.to_string(), "tests failed");
        assert_eq!(
            FailureCategory::ArchitectRejected.to_string(),
            "architect rejected"
        );
        assert_eq!(
            FailureCategory::MaxAttemptsExceeded.to_string(),
            "max attempts exceeded"
        );
        assert_eq!(
            FailureCategory::BudgetExceeded.to_string(),
            "budget exceeded"
        );
        assert_eq!(FailureCategory::Timeout.to_string(), "timeout");
        assert_eq!(FailureCategory::Other.to_string(), "other");
    }

    // -----------------------------------------------------------------------
    // Decomposition depth tests
    // -----------------------------------------------------------------------

    #[test]
    fn extract_sub_tasks_propagates_depth() {
        use glitchlab_kernel::agent::AgentMetadata;

        let plan_data = serde_json::json!({
            "decomposition": [
                { "id": "child-1", "objective": "Do A", "depends_on": [] },
                { "id": "child-2", "objective": "Do B", "depends_on": ["child-1"] }
            ]
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

        // Parent at depth 2 → children should be depth 3.
        let sub_tasks = Orchestrator::extract_sub_tasks("parent", 2, &stage_outputs).unwrap();
        assert_eq!(sub_tasks.len(), 2);
        assert_eq!(sub_tasks[0].decomposition_depth, 3);
        assert_eq!(sub_tasks[1].decomposition_depth, 3);
    }

    // -----------------------------------------------------------------------
    // handle_decomposition tests
    // -----------------------------------------------------------------------

    fn decomposition_stage_outputs() -> HashMap<String, AgentOutput> {
        use glitchlab_kernel::agent::AgentMetadata;

        let plan_data = serde_json::json!({
            "decomposition": [
                { "id": "sub-1", "objective": "Sub task A", "depends_on": [] },
                { "id": "sub-2", "objective": "Sub task B", "depends_on": ["sub-1"] }
            ]
        });
        let mut stage_outputs = HashMap::new();
        stage_outputs.insert(
            "plan".into(),
            AgentOutput {
                data: plan_data,
                metadata: AgentMetadata {
                    agent: "planner".into(),
                    model: "test".into(),
                    tokens: 100,
                    cost: 0.01,
                    latency_ms: 50,
                },
                parse_error: false,
            },
        );
        stage_outputs
    }

    fn empty_orchestrator_result() -> OrchestratorResult {
        OrchestratorResult {
            tasks_attempted: 0,
            tasks_succeeded: 0,
            tasks_failed: 0,
            tasks_deferred: 0,
            tasks_escalated: 0,
            total_cost: 0.0,
            total_tokens: 0,
            duration: Duration::from_secs(0),
            cease_reason: CeaseReason::AllTasksDone,
            run_results: Vec::new(),
            dominant_failure_pattern: None,
        }
    }

    #[test]
    fn handle_decomposition_depth_exceeded_marks_failed() {
        let tasks = vec![Task {
            id: "deep-task".into(),
            objective: "Too deep".into(),
            priority: 1,
            status: TaskStatus::InProgress,
            depends_on: vec![],
            decomposition_depth: 3,
            error: None,
            pr_url: None,
            outcome_context: None,
            remediation_depth: 0,
            is_remediation: false,
            files_hint: None,
        }];
        let mut queue = TaskQueue::from_tasks(tasks);
        let mut result = empty_orchestrator_result();
        let stage_outputs = decomposition_stage_outputs();

        let ids = Orchestrator::handle_decomposition(
            &mut queue,
            &mut result,
            "deep-task",
            3, // task_depth
            3, // max_depth — at limit
            &stage_outputs,
        );

        assert!(ids.is_empty());
        assert_eq!(result.tasks_failed, 1);
        let task = queue.tasks().iter().find(|t| t.id == "deep-task").unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(task.error.as_ref().unwrap().contains("decomposition depth"));
    }

    #[test]
    fn handle_decomposition_success_injects_sub_tasks() {
        let tasks = vec![Task {
            id: "parent-task".into(),
            objective: "Decomposable".into(),
            priority: 1,
            status: TaskStatus::InProgress,
            depends_on: vec![],
            decomposition_depth: 1,
            error: None,
            pr_url: None,
            outcome_context: None,
            remediation_depth: 0,
            is_remediation: false,
            files_hint: None,
        }];
        let mut queue = TaskQueue::from_tasks(tasks);
        let mut result = empty_orchestrator_result();
        let stage_outputs = decomposition_stage_outputs();

        let ids = Orchestrator::handle_decomposition(
            &mut queue,
            &mut result,
            "parent-task",
            1, // task_depth
            3, // max_depth — under limit
            &stage_outputs,
        );

        assert_eq!(ids, vec!["sub-1", "sub-2"]);
        assert_eq!(result.tasks_failed, 0);
        // Parent should be completed.
        let parent = queue
            .tasks()
            .iter()
            .find(|t| t.id == "parent-task")
            .unwrap();
        assert_eq!(parent.status, TaskStatus::Completed);
        // Sub-tasks should be injected with depth = 2.
        let sub1 = queue.tasks().iter().find(|t| t.id == "sub-1").unwrap();
        assert_eq!(sub1.decomposition_depth, 2);
        assert_eq!(sub1.status, TaskStatus::Pending);
        let sub2 = queue.tasks().iter().find(|t| t.id == "sub-2").unwrap();
        assert_eq!(sub2.decomposition_depth, 2);
        assert_eq!(sub2.depends_on, vec!["sub-1"]);
    }

    #[test]
    fn handle_decomposition_no_sub_tasks_marks_failed() {
        let tasks = vec![Task {
            id: "empty-decomp".into(),
            objective: "No subs".into(),
            priority: 1,
            status: TaskStatus::InProgress,
            depends_on: vec![],
            decomposition_depth: 0,
            error: None,
            pr_url: None,
            outcome_context: None,
            remediation_depth: 0,
            is_remediation: false,
            files_hint: None,
        }];
        let mut queue = TaskQueue::from_tasks(tasks);
        let mut result = empty_orchestrator_result();
        let empty_outputs = HashMap::new(); // no "plan" stage → no sub-tasks

        let ids = Orchestrator::handle_decomposition(
            &mut queue,
            &mut result,
            "empty-decomp",
            0,
            3,
            &empty_outputs,
        );

        assert!(ids.is_empty());
        assert_eq!(result.tasks_failed, 1);
        let task = queue
            .tasks()
            .iter()
            .find(|t| t.id == "empty-decomp")
            .unwrap();
        assert_eq!(task.status, TaskStatus::Failed);
        assert!(task.error.as_ref().unwrap().contains("no sub-tasks"));
    }

    // -----------------------------------------------------------------------
    // RestartIntensityMonitor tests
    // -----------------------------------------------------------------------

    #[test]
    fn restart_intensity_monitor_below_threshold() {
        let mut monitor = RestartIntensityMonitor::new(5, 300);
        // 4 failures should not trigger.
        for _ in 0..4 {
            assert!(
                monitor
                    .record_failure(FailureCategory::TestsFailed)
                    .is_none()
            );
        }
    }

    #[test]
    fn restart_intensity_monitor_at_threshold() {
        let mut monitor = RestartIntensityMonitor::new(5, 300);
        for _ in 0..4 {
            assert!(
                monitor
                    .record_failure(FailureCategory::TestsFailed)
                    .is_none()
            );
        }
        // 5th failure triggers and returns dominant category.
        let dominant = monitor.record_failure(FailureCategory::TestsFailed);
        assert_eq!(dominant, Some(FailureCategory::TestsFailed));
    }

    #[test]
    fn restart_intensity_monitor_window_expiry() {
        let mut monitor = RestartIntensityMonitor::new(3, 1);
        // Record 2 failures.
        assert!(
            monitor
                .record_failure(FailureCategory::SetupError)
                .is_none()
        );
        assert!(
            monitor
                .record_failure(FailureCategory::SetupError)
                .is_none()
        );
        // Sleep past the window.
        std::thread::sleep(Duration::from_millis(1100));
        // Old failures should have expired; this is only the 1st in the new window.
        assert!(
            monitor
                .record_failure(FailureCategory::SetupError)
                .is_none()
        );
    }

    #[test]
    fn restart_intensity_monitor_single_failure_threshold() {
        let mut monitor = RestartIntensityMonitor::new(1, 300);
        // First failure immediately triggers.
        let dominant = monitor.record_failure(FailureCategory::PlanFailed);
        assert_eq!(dominant, Some(FailureCategory::PlanFailed));
    }

    #[test]
    fn restart_intensity_monitor_dominant_category() {
        let mut monitor = RestartIntensityMonitor::new(5, 300);
        // Mix of categories: 3 tests, 2 setup
        monitor.record_failure(FailureCategory::TestsFailed);
        monitor.record_failure(FailureCategory::SetupError);
        monitor.record_failure(FailureCategory::TestsFailed);
        monitor.record_failure(FailureCategory::SetupError);
        let dominant = monitor.record_failure(FailureCategory::TestsFailed);
        assert_eq!(dominant, Some(FailureCategory::TestsFailed));
    }

    #[test]
    fn restart_intensity_monitor_mixed_categories_window_eviction() {
        let mut monitor = RestartIntensityMonitor::new(3, 1);
        // Record 2 setup errors, sleep, then record 3 test failures.
        monitor.record_failure(FailureCategory::SetupError);
        monitor.record_failure(FailureCategory::SetupError);
        std::thread::sleep(Duration::from_millis(1100));
        monitor.record_failure(FailureCategory::TestsFailed);
        monitor.record_failure(FailureCategory::TestsFailed);
        let dominant = monitor.record_failure(FailureCategory::TestsFailed);
        // Old setup errors should have expired — dominant is now TestsFailed.
        assert_eq!(dominant, Some(FailureCategory::TestsFailed));
    }

    #[test]
    fn failure_category_serde() {
        for cat in [
            FailureCategory::SetupError,
            FailureCategory::PlanFailed,
            FailureCategory::ImplementationFailed,
            FailureCategory::TestsFailed,
            FailureCategory::ArchitectRejected,
            FailureCategory::MaxAttemptsExceeded,
            FailureCategory::BudgetExceeded,
            FailureCategory::Timeout,
            FailureCategory::Other,
        ] {
            let json = serde_json::to_string(&cat).unwrap();
            let parsed: FailureCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, cat);
        }
    }

    #[test]
    fn pipeline_status_to_failure_category_mapping() {
        assert_eq!(
            pipeline_status_to_failure_category(PipelineStatus::PlanFailed),
            FailureCategory::PlanFailed
        );
        assert_eq!(
            pipeline_status_to_failure_category(PipelineStatus::ImplementationFailed),
            FailureCategory::ImplementationFailed
        );
        assert_eq!(
            pipeline_status_to_failure_category(PipelineStatus::TestsFailed),
            FailureCategory::TestsFailed
        );
        assert_eq!(
            pipeline_status_to_failure_category(PipelineStatus::ArchitectRejected),
            FailureCategory::ArchitectRejected
        );
        assert_eq!(
            pipeline_status_to_failure_category(PipelineStatus::BudgetExceeded),
            FailureCategory::BudgetExceeded
        );
        assert_eq!(
            pipeline_status_to_failure_category(PipelineStatus::TimedOut),
            FailureCategory::Timeout
        );
        assert_eq!(
            pipeline_status_to_failure_category(PipelineStatus::Error),
            FailureCategory::Other
        );
    }

    #[test]
    fn orchestrator_result_with_dominant_failure_pattern_serde() {
        let result = OrchestratorResult {
            tasks_attempted: 3,
            tasks_succeeded: 0,
            tasks_failed: 3,
            tasks_deferred: 0,
            tasks_escalated: 0,
            total_cost: 3.0,
            total_tokens: 30000,
            duration: Duration::from_millis(500),
            cease_reason: CeaseReason::SystemicFailure,
            run_results: vec![],
            dominant_failure_pattern: Some(FailureCategory::SetupError),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("dominant_failure_pattern"));
        assert!(json.contains("setup_error"));
        let parsed: OrchestratorResult = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.dominant_failure_pattern,
            Some(FailureCategory::SetupError)
        );
    }

    // -----------------------------------------------------------------------
    // CeaseReason::SystemicFailure tests
    // -----------------------------------------------------------------------

    #[test]
    fn cease_reason_systemic_failure_display() {
        assert_eq!(CeaseReason::SystemicFailure.to_string(), "systemic failure");
    }

    #[test]
    fn cease_reason_systemic_failure_serde() {
        let json = serde_json::to_string(&CeaseReason::SystemicFailure).unwrap();
        let parsed: CeaseReason = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, CeaseReason::SystemicFailure);
    }

    // -----------------------------------------------------------------------
    // Orchestrator systemic failure integration test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn orchestrator_inline_tqm_injects_remediation() {
        // This test verifies that when remediation_enabled is true, the inline
        // TQM check can detect patterns and inject remediation tasks mid-run.
        // Tasks fail (no API keys) with Error status. With threshold=1, the
        // stuck_agents detector fires after the first failure, injecting a
        // remediation task into the queue.
        let mut config = EngConfig::default();
        config.tqm.remediation_enabled = true;
        config.tqm.stuck_agents_threshold = 1;
        // Set restart intensity high so it doesn't halt before TQM fires.
        config.limits.restart_intensity_max_failures = 20;
        config.limits.max_fix_attempts = 1;

        let handler = Arc::new(AutoApproveHandler);
        let dir = tempfile::tempdir().unwrap();
        let history: Arc<dyn HistoryBackend> =
            Arc::new(glitchlab_memory::history::JsonlHistory::new(dir.path()));

        let orchestrator = Orchestrator::new(config, handler, history);
        let tasks = vec![
            pending_task("tqm1", "task A"),
            pending_task("tqm2", "task B"),
            pending_task("tqm3", "task C"),
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
        // Tasks should have been attempted (and failed due to no API keys).
        assert!(result.tasks_attempted >= 1);
        // Check that TQM remediation tasks were injected into the queue.
        let has_tqm_task = queue.tasks().iter().any(|t| t.id.starts_with("tqm-"));
        assert!(has_tqm_task, "expected TQM remediation tasks in queue");
    }

    #[tokio::test]
    async fn orchestrator_max_total_tasks_halts() {
        let mut config = EngConfig::default();
        config.limits.max_total_tasks = 5; // set very low

        let handler = Arc::new(AutoApproveHandler);
        let dir = tempfile::tempdir().unwrap();
        let history: Arc<dyn HistoryBackend> =
            Arc::new(glitchlab_memory::history::JsonlHistory::new(dir.path()));

        let orchestrator = Orchestrator::new(config, handler, history);
        // 6 tasks exceeds the limit of 5
        let tasks = vec![
            pending_task("mt1", "task 1"),
            pending_task("mt2", "task 2"),
            pending_task("mt3", "task 3"),
            pending_task("mt4", "task 4"),
            pending_task("mt5", "task 5"),
            pending_task("mt6", "task 6"),
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
        assert_eq!(result.cease_reason, CeaseReason::SystemicFailure);
        assert_eq!(result.tasks_attempted, 0);
    }

    #[tokio::test]
    async fn orchestrator_systemic_failure_halts() {
        let mut config = EngConfig::default();
        // Set intensity threshold to 3 failures in 300s.
        config.limits.restart_intensity_max_failures = 3;
        config.limits.restart_intensity_window_secs = 300;

        let handler = Arc::new(AutoApproveHandler);
        let dir = tempfile::tempdir().unwrap();
        let history: Arc<dyn HistoryBackend> =
            Arc::new(glitchlab_memory::history::JsonlHistory::new(dir.path()));

        let orchestrator = Orchestrator::new(config, handler, history);
        // 5 tasks — but orchestrator should halt after 3 failures.
        let tasks = vec![
            pending_task("sf1", "task 1"),
            pending_task("sf2", "task 2"),
            pending_task("sf3", "task 3"),
            pending_task("sf4", "task 4"),
            pending_task("sf5", "task 5"),
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
        assert_eq!(result.cease_reason, CeaseReason::SystemicFailure);
        // Should have attempted exactly 3 tasks (the threshold).
        assert_eq!(result.tasks_attempted, 3);
        assert_eq!(result.tasks_failed, 3);
        // Remaining tasks should still be pending.
        let t4 = queue.tasks().iter().find(|t| t.id == "sf4").unwrap();
        assert_eq!(t4.status, TaskStatus::Pending);
        let t5 = queue.tasks().iter().find(|t| t.id == "sf5").unwrap();
        assert_eq!(t5.status, TaskStatus::Pending);
    }

    // -----------------------------------------------------------------------
    // Circuit integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn circuit_output_to_tasks_basic() {
        let output = serde_json::json!({
            "diagnosis": "boundary check missing",
            "root_cause": "protected path not checked in pipeline",
            "remediation_tasks": [
                {
                    "objective": "Add protected-path check in pipeline.rs execute()",
                    "target_files": ["crates/eng-org/src/pipeline.rs"],
                    "estimated_complexity": "small",
                    "rationale": "The boundary check is missing"
                }
            ],
            "scope_assessment": { "within_scope": true, "modifies_kernel": false },
            "suggested_priority": 2,
            "confidence": "high",
            "escalate": false
        });

        let tqm_config = crate::tqm::TQMConfig::default();
        let tasks = Orchestrator::circuit_output_to_tasks(&output, &tqm_config, 1);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].is_remediation);
        assert_eq!(tasks[0].remediation_depth, 1);
        assert_eq!(tasks[0].priority, 2);
        assert!(tasks[0].id.starts_with("circuit-fix-"));
    }

    #[test]
    fn circuit_escalation_skips() {
        let output = serde_json::json!({
            "diagnosis": "unclear",
            "root_cause": "unknown",
            "remediation_tasks": [],
            "scope_assessment": { "within_scope": true, "modifies_kernel": false },
            "suggested_priority": 5,
            "confidence": "low",
            "escalate": true
        });

        let tqm_config = crate::tqm::TQMConfig::default();
        let tasks = Orchestrator::circuit_output_to_tasks(&output, &tqm_config, 1);
        assert!(tasks.is_empty());
    }

    #[test]
    fn circuit_out_of_scope_skips() {
        let output = serde_json::json!({
            "diagnosis": "kernel bug",
            "root_cause": "kernel issue",
            "remediation_tasks": [
                { "objective": "fix kernel", "target_files": ["crates/kernel/src/lib.rs"] }
            ],
            "scope_assessment": { "within_scope": false, "modifies_kernel": true },
            "suggested_priority": 1,
            "confidence": "high",
            "escalate": false
        });

        let tqm_config = crate::tqm::TQMConfig::default();
        let tasks = Orchestrator::circuit_output_to_tasks(&output, &tqm_config, 1);
        assert!(tasks.is_empty());
    }

    #[test]
    fn meta_loop_guard() {
        // max_remediation_depth = 0 → no remediation tasks generated
        let output = serde_json::json!({
            "diagnosis": "test",
            "root_cause": "test",
            "remediation_tasks": [
                { "objective": "fix something", "target_files": ["foo.rs"] }
            ],
            "scope_assessment": { "within_scope": true, "modifies_kernel": false },
            "suggested_priority": 2,
            "confidence": "high",
            "escalate": false
        });

        let tqm_config = crate::tqm::TQMConfig::default();
        let tasks = Orchestrator::circuit_output_to_tasks(&output, &tqm_config, 0);
        assert!(tasks.is_empty());
    }

    #[test]
    fn fallback_on_circuit_failure() {
        // When Circuit output is garbage / missing escalate, defaults to escalate=true
        let output = serde_json::json!({ "garbage": true });
        let tqm_config = crate::tqm::TQMConfig::default();
        let tasks = Orchestrator::circuit_output_to_tasks(&output, &tqm_config, 1);
        assert!(tasks.is_empty());
    }

    #[test]
    fn repair_budget_skip() {
        // Remediation task should be skipped if repair budget is exhausted.
        let tasks = vec![crate::taskqueue::Task {
            id: "fix-1".into(),
            objective: "Remediation".into(),
            priority: 1,
            status: TaskStatus::Pending,
            depends_on: vec![],
            decomposition_depth: 0,
            error: None,
            pr_url: None,
            outcome_context: None,
            remediation_depth: 1,
            is_remediation: true,
            files_hint: None,
        }];
        let queue = TaskQueue::from_tasks(tasks);
        let next = queue.pick_next().unwrap();
        // Simulate budget exhaustion check:
        let budget = CumulativeBudget::new(0.01).with_repair_budget_fraction(0.01);
        assert!(!budget.can_afford_repair(0.50)); // per-task cost exceeds repair budget
        assert!(next.is_remediation);
    }
}
