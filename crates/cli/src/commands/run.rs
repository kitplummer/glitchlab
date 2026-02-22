use std::path::Path;

use anyhow::{Context, Result, bail};
use glitchlab_eng_org::config::EngConfig;
use serde::Deserialize;
use tokio::process::Command;

use super::common;

// ---------------------------------------------------------------------------
// Task file deserialization
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TaskFile {
    id: String,
    objective: String,
}

// ---------------------------------------------------------------------------
// Arguments bundle
// ---------------------------------------------------------------------------

pub struct RunArgs<'a> {
    pub repo: &'a Path,
    pub issue: Option<u32>,
    pub local_task: bool,
    pub task_file: Option<&'a Path>,
    pub objective: Option<&'a str>,
    pub allow_core: bool,
    pub auto_approve: bool,
    pub test: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// Main execute function
// ---------------------------------------------------------------------------

pub async fn execute(args: RunArgs<'_>) -> Result<()> {
    // --- Resolve task ---
    let (task_id, obj) = resolve_task(
        args.repo,
        args.issue,
        args.local_task,
        args.task_file,
        args.objective,
    )
    .await?;

    eprintln!("task: {task_id}");
    eprintln!("objective: {obj}");
    eprintln!();

    // --- Load config ---
    let mut config = EngConfig::load(Some(args.repo))
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to load config")?;

    if args.allow_core {
        config.boundaries.protected_paths.clear();
    }

    if args.auto_approve {
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.intervention.pause_on_core_change = false;
        config.intervention.pause_on_budget_exceeded = false;
    }

    if let Some(cmd) = args.test {
        config.test_command_override = Some(cmd.to_string());
    }

    // --- Setup & run pipeline ---
    let (_router, pipeline) = common::setup_pipeline(&config, args.auto_approve)?;
    common::run_pipeline(&pipeline, &task_id, &obj, args.repo).await
}

// ---------------------------------------------------------------------------
// Task resolution
// ---------------------------------------------------------------------------

async fn resolve_task(
    repo: &Path,
    issue: Option<u32>,
    local_task: bool,
    task_file: Option<&Path>,
    objective: Option<&str>,
) -> Result<(String, String)> {
    if let Some(path) = task_file {
        let contents = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read task file: {}", path.display()))?;
        let task: TaskFile =
            serde_yaml::from_str(&contents).context("failed to parse task YAML")?;
        return Ok((task.id, task.objective));
    }

    if let Some(text) = objective {
        return Ok(("inline".into(), text.to_string()));
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
        let obj = fetch_issue_objective(repo, num).await?;
        return Ok((format!("issue-{num}"), obj));
    }

    bail!("no task specified — use --task-file, --objective, --local-task, or --issue");
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
