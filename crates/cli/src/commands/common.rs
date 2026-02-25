use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use glitchlab_eng_org::config::EngConfig;
use glitchlab_eng_org::pipeline::{
    AutoApproveHandler, EngineeringPipeline, InterventionHandler, RealExternalOps,
};
use glitchlab_kernel::budget::BudgetTracker;
use glitchlab_kernel::pipeline::{PipelineResult, PipelineStatus};
use glitchlab_memory::history::HistoryBackend;
use glitchlab_router::Router;
use tokio::process::Command;

// ---------------------------------------------------------------------------
// CLI approval handler (interactive stdin y/N)
// ---------------------------------------------------------------------------

pub struct CliApprovalHandler;

impl InterventionHandler for CliApprovalHandler {
    fn request_approval(
        &self,
        gate: &str,
        summary: &str,
        _data: &serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        let gate = gate.to_string();
        let summary = summary.to_string();
        Box::pin(async move {
            eprintln!("\n--- Intervention gate: {gate} ---");
            eprintln!("{summary}");
            eprint!("Approve? [y/N] ");

            let answer = tokio::task::spawn_blocking(|| {
                let mut input = String::new();
                std::io::stdin().read_line(&mut input).ok();
                input
            })
            .await
            .unwrap_or_default();

            matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
        })
    }
}

// ---------------------------------------------------------------------------
// Pipeline setup
// ---------------------------------------------------------------------------

/// Build a `Router` and `EngineeringPipeline` from the loaded config.
pub async fn setup_pipeline(
    config: &EngConfig,
    auto_approve: bool,
    repo_path: &Path,
) -> Result<(Arc<Router>, EngineeringPipeline)> {
    // --- Build router ---
    let budget = BudgetTracker::new(
        config.limits.max_tokens_per_task,
        config.limits.max_dollars_per_task,
    );
    let provider_inits = config.resolve_providers();
    let mut router = Router::with_providers(config.routing_map(), budget, provider_inits);
    if let Some(chooser) = config.build_chooser() {
        router = router.with_chooser(chooser);
    }

    // --- Pre-flight check ---
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

    let router = Arc::new(router);

    // --- Build history backend ---
    let history = build_history_backend(repo_path, config).await;

    // --- Build pipeline ---
    let handler: Arc<dyn InterventionHandler> = if auto_approve {
        Arc::new(AutoApproveHandler)
    } else {
        Arc::new(CliApprovalHandler)
    };

    let pipeline = EngineeringPipeline::new(
        Arc::clone(&router),
        config.clone(),
        handler,
        history,
        Arc::new(RealExternalOps),
    );

    Ok((router, pipeline))
}

/// Build the history backend from config.
async fn build_history_backend(repo_path: &Path, config: &EngConfig) -> Arc<dyn HistoryBackend> {
    let mem_config = config.memory.to_backend_config();
    glitchlab_memory::build_backend(repo_path, &mem_config).await
}

// ---------------------------------------------------------------------------
// Pipeline execution
// ---------------------------------------------------------------------------

/// Run the pipeline for a single task and handle result display / exit code.
pub async fn run_pipeline(
    pipeline: &EngineeringPipeline,
    task_id: &str,
    objective: &str,
    repo: &Path,
) -> Result<()> {
    // --- Detect base branch ---
    let base_branch = detect_base_branch(repo).await?;

    // --- Run ---
    eprintln!("starting pipeline (base branch: {base_branch})...");
    let result = pipeline
        .run(task_id, objective, repo, &base_branch, &[])
        .await;

    // --- Display results ---
    print_result(&result);

    // --- Exit code ---
    match result.status {
        PipelineStatus::PrCreated | PipelineStatus::Committed => Ok(()),
        _ => {
            let msg = result
                .error
                .unwrap_or_else(|| format!("{:?}", result.status));
            bail!("pipeline failed: {msg}");
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub async fn detect_base_branch(repo: &Path) -> Result<String> {
    for branch in &["main", "master"] {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", branch])
            .current_dir(repo)
            .output()
            .await;

        if let Ok(o) = output
            && o.status.success()
        {
            return Ok((*branch).to_string());
        }
    }

    // Fallback: use current branch.
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(repo)
        .output()
        .await
        .context("failed to detect base branch")?;

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        bail!("could not detect base branch");
    }
    Ok(branch)
}

pub fn print_result(result: &PipelineResult) {
    eprintln!();
    eprintln!("=== Pipeline Result ===");
    eprintln!("status: {:?}", result.status);

    if let Some(ref url) = result.pr_url {
        eprintln!("pr:     {url}");
    }
    if let Some(ref branch) = result.branch {
        eprintln!("branch: {branch}");
    }
    if let Some(ref err) = result.error {
        eprintln!("error:  {err}");
    }

    eprintln!();
    eprintln!(
        "budget: {} tokens, ${:.4}, {} calls",
        result.budget.total_tokens, result.budget.estimated_cost, result.budget.call_count
    );
    eprintln!(
        "remaining: {} tokens, ${:.2}",
        result.budget.tokens_remaining, result.budget.dollars_remaining
    );
    eprintln!("events: {}", result.events.len());
}
