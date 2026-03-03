use std::path::Path;

use anyhow::{Context, Result};
use glitchlab_eng_org::config::EngConfig;
use glitchlab_eng_org::taskqueue::TaskQueue;

use super::common;

// ---------------------------------------------------------------------------
// Execute
// ---------------------------------------------------------------------------

pub async fn execute(
    repo: &Path,
    allow_core: bool,
    auto_approve: bool,
    test: Option<&str>,
) -> Result<()> {
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

    // --- Find next pending task ---
    let tasks_path = repo.join(".glitchlab/tasks/backlog.yaml");
    let queue = TaskQueue::load(&tasks_path)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("failed to load task queue from {}", tasks_path.display()))?;

    let task = queue
        .pick_next()
        .ok_or_else(|| anyhow::anyhow!("no pending tasks in the backlog"))?;

    let task_id = task.id.clone();
    let objective = task.objective.clone();

    eprintln!("task: {task_id}");
    eprintln!("objective: {objective}");
    eprintln!();

    // --- Setup & run pipeline ---
    let (_router, pipeline) = common::setup_pipeline(&config, auto_approve, repo).await?;
    common::run_pipeline(&pipeline, &task_id, &objective, repo).await
}
