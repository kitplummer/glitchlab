use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use glitchlab_eng_org::config::EngConfig;
use glitchlab_eng_org::orchestrator::{
    CumulativeBudget, Orchestrator, OrchestratorParams, OrchestratorResult,
};
use glitchlab_eng_org::pipeline::{AutoApproveHandler, InterventionHandler};
use glitchlab_eng_org::taskqueue::TaskQueue;
use glitchlab_memory::history::HistoryBackend;

use super::common;

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

pub struct BatchArgs<'a> {
    pub repo: &'a Path,
    pub budget: f64,
    pub tasks_file: Option<&'a Path>,
    pub auto_approve: bool,
    pub test: Option<&'a str>,
    pub quality_gate: Option<&'a str>,
    pub stop_on_failure: bool,
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// Execute
// ---------------------------------------------------------------------------

pub async fn execute(args: BatchArgs<'_>) -> Result<()> {
    // --- Load config ---
    let mut config = EngConfig::load(Some(args.repo))
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to load config")?;

    // Batch mode always disables intervention gates.
    config.intervention.pause_after_plan = false;
    config.intervention.pause_before_pr = false;
    config.intervention.pause_on_core_change = false;
    config.intervention.pause_on_budget_exceeded = false;

    if let Some(cmd) = args.test {
        config.test_command_override = Some(cmd.to_string());
    }

    // --- Load task queue ---
    let tasks_path = match args.tasks_file {
        Some(p) => p.to_path_buf(),
        None => args.repo.join(".glitchlab/tasks/backlog.yaml"),
    };
    let mut queue = TaskQueue::load(&tasks_path)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("failed to load task queue from {}", tasks_path.display()))?;

    let summary = queue.summary();
    eprintln!(
        "task queue: {} total, {} pending",
        summary.total, summary.pending
    );
    if summary.pending == 0 {
        eprintln!("no pending tasks — nothing to do");
        return Ok(());
    }

    // --- Dry run mode ---
    if args.dry_run {
        eprintln!();
        eprintln!("=== DRY RUN MODE ===");
        eprintln!(
            "Budget per task: ${:.2}",
            config.limits.max_dollars_per_task
        );
        eprintln!();

        // Show pending tasks
        let pending_tasks: Vec<_> = queue
            .tasks()
            .iter()
            .filter(|task| task.status == glitchlab_eng_org::taskqueue::TaskStatus::Pending)
            .collect();

        if pending_tasks.is_empty() {
            eprintln!("No pending tasks to display.");
        } else {
            eprintln!("Pending tasks:");
            for task in pending_tasks {
                // Truncate objective to 80 characters
                let objective = if task.objective.len() > 80 {
                    format!("{}...", &task.objective[..77])
                } else {
                    task.objective.clone()
                };

                eprintln!(
                    "  [{}] {} (${:.2})",
                    task.id, objective, config.limits.max_dollars_per_task
                );
            }
        }

        eprintln!();
        eprintln!("Dry run complete. No tasks were executed.");
        return Ok(());
    }

    // --- Preflight check ---
    let provider_inits = config.resolve_providers();
    let budget_tracker = glitchlab_kernel::budget::BudgetTracker::new(
        config.limits.max_tokens_per_task,
        config.limits.max_dollars_per_task,
    );
    let mut router = glitchlab_router::Router::with_providers(
        config.routing_map(),
        budget_tracker,
        provider_inits,
    );
    if let Some(chooser) = config.build_chooser() {
        router = router.with_chooser(chooser);
    }
    let preflight_errors = router.preflight_check();
    if !preflight_errors.is_empty() {
        eprintln!("Pre-flight check failed:");
        for (role, msg) in &preflight_errors {
            eprintln!("  [{role}] {msg}");
        }
        bail!(
            "missing API keys for {} role(s). Set the required environment variables and retry.",
            preflight_errors.len()
        );
    }

    // --- Cumulative budget ---
    let ledger_path = args.repo.join(".glitchlab/logs/budget_ledger.jsonl");
    let mut budget = CumulativeBudget::with_ledger(args.budget, &ledger_path);

    eprintln!(
        "budget: ${:.2} total, ${:.2} remaining (${:.2} per task)",
        args.budget,
        budget.remaining_dollars(),
        config.limits.max_dollars_per_task
    );
    eprintln!();

    // --- Detect base branch ---
    let base_branch = common::detect_base_branch(args.repo).await?;

    // --- Build orchestrator ---
    let handler: Arc<dyn InterventionHandler> = if args.auto_approve {
        Arc::new(AutoApproveHandler)
    } else {
        Arc::new(common::CliApprovalHandler)
    };

    let mem_config = config.memory.to_backend_config();
    let history: Arc<dyn HistoryBackend> =
        glitchlab_memory::build_backend(args.repo, &mem_config).await;

    let orchestrator = Orchestrator::new(config.clone(), handler, history);
    let params = OrchestratorParams {
        repo_path: args.repo.to_path_buf(),
        base_branch,
        quality_gate_command: args.quality_gate.map(String::from),
        stop_on_failure: args.stop_on_failure,
    };

    // --- Run ---
    eprintln!("starting orchestrator...");
    let result = orchestrator.run(&mut queue, &mut budget, &params).await;

    // --- Display results ---
    print_result(&result);

    // --- Final task queue state ---
    let final_summary = queue.summary();
    eprintln!();
    eprintln!("=== Task Queue ===");
    eprintln!(
        "completed: {}, failed: {}, pending: {}, skipped: {}",
        final_summary.completed, final_summary.failed, final_summary.pending, final_summary.skipped
    );

    if result.tasks_failed > 0 {
        eprintln!(
            "note: {} task(s) failed (see details above)",
            result.tasks_failed
        );
    }

    Ok(())
}

fn print_result(result: &OrchestratorResult) {
    eprintln!();
    eprintln!("=== Orchestrator Result ===");
    eprintln!("cease reason: {:?}", result.cease_reason);
    eprintln!(
        "tasks: {} attempted, {} succeeded, {} failed",
        result.tasks_attempted, result.tasks_succeeded, result.tasks_failed
    );
    eprintln!(
        "budget: {} tokens, ${:.4}",
        result.total_tokens, result.total_cost
    );

    if !result.run_results.is_empty() {
        eprintln!();
        eprintln!("--- Task Results ---");
        for run in &result.run_results {
            let status_indicator = if run.error.is_some() { "FAIL" } else { "OK" };
            eprintln!(
                "  [{status_indicator}] {} — {} (${:.4}, {} tokens)",
                run.task_id, run.status, run.cost, run.tokens
            );
            if let Some(ref url) = run.pr_url {
                eprintln!("        PR: {url}");
            }
            if let Some(ref err) = run.error {
                eprintln!("        error: {err}");
            }
        }
    }
}
