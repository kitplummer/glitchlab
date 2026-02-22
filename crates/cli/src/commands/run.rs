use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use glitchlab_eng_org::config::EngConfig;
use glitchlab_eng_org::pipeline::{AutoApproveHandler, EngineeringPipeline, InterventionHandler};
use glitchlab_kernel::budget::BudgetTracker;
use glitchlab_kernel::pipeline::{PipelineResult, PipelineStatus};
use glitchlab_router::Router;
use serde::Deserialize;
use tokio::process::Command;

// ---------------------------------------------------------------------------
// Task file deserialization
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TaskFile {
    id: String,
    objective: String,
}

// ---------------------------------------------------------------------------
// CLI approval handler (interactive stdin y/N)
// ---------------------------------------------------------------------------

struct CliApprovalHandler;

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
// Main execute function
// ---------------------------------------------------------------------------

pub async fn execute(
    repo: &Path,
    issue: Option<u32>,
    local_task: bool,
    task_file: Option<&Path>,
    allow_core: bool,
    auto_approve: bool,
    test: Option<&str>,
) -> Result<()> {
    // --- Resolve task ---
    let (task_id, objective) = resolve_task(repo, issue, local_task, task_file).await?;

    eprintln!("task: {task_id}");
    eprintln!("objective: {objective}");
    eprintln!();

    // --- Load config ---
    let mut config = EngConfig::load(Some(repo))
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to load config")?;

    if allow_core {
        config.boundaries.protected_paths.clear();
    }

    if auto_approve {
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.intervention.pause_on_core_change = false;
        config.intervention.pause_on_budget_exceeded = false;
    }

    if let Some(cmd) = test {
        config.test_command_override = Some(cmd.to_string());
    }

    // --- Build router ---
    let budget = BudgetTracker::new(
        config.limits.max_tokens_per_task,
        config.limits.max_dollars_per_task,
    );
    let router = Router::new(config.routing_map(), budget);

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

    // --- Build pipeline ---
    let handler: Arc<dyn InterventionHandler> = if auto_approve {
        Arc::new(AutoApproveHandler)
    } else {
        Arc::new(CliApprovalHandler)
    };

    let pipeline = EngineeringPipeline::new(Arc::clone(&router), config, handler);

    // --- Detect base branch ---
    let base_branch = detect_base_branch(repo).await?;

    // --- Run ---
    eprintln!("starting pipeline (base branch: {base_branch})...");
    let result = pipeline.run(&task_id, &objective, repo, &base_branch).await;

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
// Task resolution
// ---------------------------------------------------------------------------

async fn resolve_task(
    repo: &Path,
    issue: Option<u32>,
    local_task: bool,
    task_file: Option<&Path>,
) -> Result<(String, String)> {
    if let Some(path) = task_file {
        let contents = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read task file: {}", path.display()))?;
        let task: TaskFile =
            serde_yaml::from_str(&contents).context("failed to parse task YAML")?;
        return Ok((task.id, task.objective));
    }

    if local_task {
        let path = repo.join(".glitchlab").join("tasks").join("next.yaml");
        let contents = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        let task: TaskFile =
            serde_yaml::from_str(&contents).context("failed to parse task YAML")?;
        return Ok((task.id, task.objective));
    }

    if let Some(num) = issue {
        let objective = fetch_issue_objective(repo, num).await?;
        return Ok((format!("issue-{num}"), objective));
    }

    bail!("no task specified — use --issue, --local-task, or --task-file");
}

async fn fetch_issue_objective(repo: &Path, issue_number: u32) -> Result<String> {
    let output = Command::new("gh")
        .args([
            "issue",
            "view",
            &issue_number.to_string(),
            "--json",
            "title,body",
        ])
        .current_dir(repo)
        .output()
        .await
        .context("failed to run `gh issue view`")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh issue view failed: {stderr}");
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("failed to parse gh output")?;

    let title = json["title"].as_str().unwrap_or("untitled");
    let body = json["body"].as_str().unwrap_or("");

    if body.is_empty() {
        Ok(title.to_string())
    } else {
        Ok(format!("{title}\n\n{body}"))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn detect_base_branch(repo: &Path) -> Result<String> {
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

fn print_result(result: &PipelineResult) {
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
