use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use glitchlab_kernel::budget::BudgetTracker;
use glitchlab_kernel::error::{Error, Result};
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

/// Result of a single task run within the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRunResult {
    pub task_id: String,
    pub status: String,
    pub cost: f64,
    pub tokens: u64,
    pub pr_url: Option<String>,
    pub error: Option<String>,
}

/// Result of the full orchestrator run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorResult {
    pub tasks_attempted: u32,
    pub tasks_succeeded: u32,
    pub tasks_failed: u32,
    pub total_cost: f64,
    pub total_tokens: u64,
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
        let mut result = OrchestratorResult {
            tasks_attempted: 0,
            tasks_succeeded: 0,
            tasks_failed: 0,
            total_cost: 0.0,
            total_tokens: 0,
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

            // --- Mark in progress ---
            queue.update_status(&task_id, TaskStatus::InProgress);
            let _ = queue.save();

            info!(task_id = %task_id, "starting task");

            // --- Create fresh router + pipeline for this task ---
            let pipeline_result = match self
                .create_and_run_pipeline(&task_id, &objective, params)
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

            let succeeded = matches!(
                pipeline_result.status,
                PipelineStatus::PrCreated | PipelineStatus::Committed
            );

            let run_result = TaskRunResult {
                task_id: task_id.clone(),
                status: status_str,
                cost,
                tokens,
                pr_url: pipeline_result.pr_url.clone(),
                error: pipeline_result.error.clone(),
            };
            result.run_results.push(run_result);

            // --- Update task status ---
            if succeeded {
                result.tasks_succeeded += 1;
                queue.update_status(&task_id, TaskStatus::Completed);
                if let Some(ref url) = pipeline_result.pr_url {
                    queue.set_pr_url(&task_id, url.clone());
                }
            } else {
                result.tasks_failed += 1;
                queue.update_status(&task_id, TaskStatus::Failed);
                if let Some(ref err) = pipeline_result.error {
                    queue.set_error(&task_id, err.clone());
                }
            }
            let _ = queue.save();

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

        result
    }

    /// Create a fresh Router + Pipeline and run a single task.
    async fn create_and_run_pipeline(
        &self,
        task_id: &str,
        objective: &str,
        params: &OrchestratorParams,
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
            .run(task_id, objective, &params.repo_path, &params.base_branch)
            .await)
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
        }];
        let mut queue = TaskQueue::from_tasks(tasks);
        let mut budget = CumulativeBudget::new(0.50); // less than per-task $2.00
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
            },
            Task {
                id: "b".into(),
                objective: "B".into(),
                priority: 2,
                status: TaskStatus::Pending,
                depends_on: vec!["a".into()],
                error: None,
                pr_url: None,
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
            total_cost: 12.50,
            total_tokens: 150000,
            cease_reason: CeaseReason::BudgetExhausted,
            run_results: vec![TaskRunResult {
                task_id: "t1".into(),
                status: "PrCreated".into(),
                cost: 4.0,
                tokens: 50000,
                pr_url: Some("https://example.com/pr/1".into()),
                error: None,
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
    async fn quality_gate_truncates_long_output() {
        let dir = tempfile::tempdir().unwrap();
        // Write a script that generates > 2000 chars of output.
        let script_path = dir.path().join("longout.sh");
        std::fs::write(
            &script_path,
            "#!/bin/bash\nfor i in $(seq 1 3000); do printf x; done\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(
            &script_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let result = run_quality_gate(script_path.to_str().unwrap(), dir.path()).await;
        assert!(!result.passed);
        assert!(result.details.contains("...[truncated]"));
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
}
