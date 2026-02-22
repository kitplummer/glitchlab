use std::io::BufRead;
use std::path::Path;

use anyhow::{Context, Result, bail};
use glitchlab_eng_org::config::EngConfig;

use super::common;

pub async fn execute(
    repo: &Path,
    allow_core: bool,
    auto_approve: bool,
    test: Option<&str>,
) -> Result<()> {
    eprintln!("glitchlab interactive — describe what you want to build");
    eprintln!("(enter your objective, then press Enter on an empty line to submit)\n");

    loop {
        let objective = read_objective()?;
        if objective.is_empty() {
            bail!("no objective provided");
        }

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

        // --- Setup & run pipeline ---
        let (_router, pipeline) = common::setup_pipeline(&config, auto_approve)?;
        common::run_pipeline(&pipeline, "interactive", &objective, repo).await?;

        // --- Continue? ---
        eprint!("Another task? [y/N] ");
        let answer = read_line()?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
            break;
        }

        eprintln!();
        eprintln!("Enter your next objective:");
    }

    Ok(())
}

/// Read a multi-line objective from stdin, terminated by an empty line or EOF.
fn read_objective() -> Result<String> {
    eprint!("> ");
    let stdin = std::io::stdin();
    let mut lines = Vec::new();

    for line in stdin.lock().lines() {
        let line = line.context("failed to read from stdin")?;
        if line.is_empty() {
            break;
        }
        lines.push(line);
    }

    Ok(lines.join("\n").trim().to_string())
}

/// Read a single line from stdin.
fn read_line() -> Result<String> {
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .context("failed to read from stdin")?;
    Ok(input)
}
