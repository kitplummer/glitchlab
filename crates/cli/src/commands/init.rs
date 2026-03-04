use std::path::Path;

use anyhow::Result;

pub async fn execute(path: &Path) -> Result<()> {
    let glitchlab_dir = path.join(".glitchlab");

    // Create directory structure.
    let dirs = [
        glitchlab_dir.join("tasks"),
        glitchlab_dir.join("worktrees"),
        glitchlab_dir.join("logs"),
    ];

    for dir in &dirs {
        tokio::fs::create_dir_all(dir).await?;
    }

    // Write .gitignore to prevent tracking ephemeral data.
    let gitignore_path = glitchlab_dir.join(".gitignore");
    if !gitignore_path.exists() {
        tokio::fs::write(&gitignore_path, "worktrees/\nlogs/\n").await?;
    }

    // Write default config if it doesn't exist.
    let config_path = glitchlab_dir.join("config.yaml");
    if !config_path.exists() {
        let default_config = r#"# GLITCHLAB configuration overrides
# Values here are merged on top of built-in defaults.

# routing:
#   implementer: "anthropic/claude-sonnet-4-20250514"

# boundaries:
#   protected_paths:
#     - "crates/core"

# limits:
#   max_fix_attempts: 4
#   max_dollars_per_task: 10.0

# workspace:
#   setup_command: "mix deps.get && mix compile"
"#;
        tokio::fs::write(&config_path, default_config).await?;
    }

    // Write example task if it doesn't exist.
    let task_path = glitchlab_dir.join("tasks").join("example.yaml");
    if !task_path.exists() {
        let example_task = r#"id: example-001
objective: "Add a --verbose flag to the CLI"
constraints:
  - "No new dependencies"
  - "Must not change existing flag behavior"
acceptance:
  - "Tests pass"
  - "New test covers the flag"
  - "Help text updated"
risk: low
"#;
        tokio::fs::write(&task_path, example_task).await?;
    }

    println!("Initialized GLITCHLAB in {}", path.display());
    println!("  Created: .glitchlab/config.yaml");
    println!("  Created: .glitchlab/tasks/example.yaml");
    println!("  Created: .glitchlab/worktrees/");
    println!("  Created: .glitchlab/logs/");

    // Initialize beads database (`bd init`). Tolerate failure gracefully if
    // `bd` is not installed — warn but don't error.
    run_bd_init(path).await;

    // Run setup_command from config if present.
    run_setup_command(path).await?;

    Ok(())
}

/// Run `bd init` in the target repo directory. Warns on failure (bd may not
/// be installed).
async fn run_bd_init(path: &Path) {
    match tokio::process::Command::new("bd")
        .arg("init")
        .current_dir(path)
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            println!("  Initialized beads database (bd init)");
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            eprintln!(
                "  Warning: bd init exited with {}: {}",
                output.status,
                stderr.trim()
            );
        }
        Err(e) => {
            eprintln!("  Warning: bd not found, skipping beads init ({e})");
        }
    }
}

/// Load config from the repo and run `setup_command` if configured.
async fn run_setup_command(path: &Path) -> Result<()> {
    let config = glitchlab_eng_org::config::EngConfig::load(Some(path))?;
    if let Some(cmd) = &config.workspace.setup_command {
        println!("  Running setup command: {cmd}");
        let status = tokio::process::Command::new("sh")
            .args(["-c", cmd])
            .current_dir(path)
            .status()
            .await?;
        if !status.success() {
            anyhow::bail!("setup command failed with {status}");
        }
        println!("  Setup command completed successfully");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_creates_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        execute(dir.path()).await.unwrap();

        let gitignore_path = dir.path().join(".glitchlab").join(".gitignore");
        assert!(gitignore_path.exists(), ".gitignore should be created");
        let content = tokio::fs::read_to_string(&gitignore_path).await.unwrap();
        assert!(content.contains("worktrees/"));
        assert!(content.contains("logs/"));
    }

    #[tokio::test]
    async fn init_creates_config_and_dirs() {
        let dir = tempfile::tempdir().unwrap();
        execute(dir.path()).await.unwrap();

        assert!(dir.path().join(".glitchlab/config.yaml").exists());
        assert!(dir.path().join(".glitchlab/tasks").is_dir());
        assert!(dir.path().join(".glitchlab/worktrees").is_dir());
        assert!(dir.path().join(".glitchlab/logs").is_dir());
    }

    #[tokio::test]
    async fn init_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        execute(dir.path()).await.unwrap();
        // Running again should not fail or overwrite.
        execute(dir.path()).await.unwrap();
        let content = tokio::fs::read_to_string(dir.path().join(".glitchlab/.gitignore"))
            .await
            .unwrap();
        assert!(content.contains("worktrees/"));
    }

    #[tokio::test]
    async fn bd_init_tolerates_missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        // Should not panic or error even if bd is not on PATH.
        run_bd_init(dir.path()).await;
    }

    #[tokio::test]
    async fn run_setup_command_skips_when_no_config() {
        let dir = tempfile::tempdir().unwrap();
        // No .glitchlab/config.yaml — should load defaults (setup_command: None)
        // and skip without error.
        run_setup_command(dir.path()).await.unwrap();
    }

    #[tokio::test]
    async fn run_setup_command_executes_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let glitchlab_dir = dir.path().join(".glitchlab");
        std::fs::create_dir_all(&glitchlab_dir).unwrap();
        let marker = dir.path().join("setup_ran.txt");
        let cmd = format!("touch {}", marker.display());
        let config = format!("workspace:\n  setup_command: \"{cmd}\"\n");
        std::fs::write(glitchlab_dir.join("config.yaml"), config).unwrap();

        run_setup_command(dir.path()).await.unwrap();
        assert!(
            marker.exists(),
            "setup command should have created marker file"
        );
    }

    #[tokio::test]
    async fn run_setup_command_errors_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let glitchlab_dir = dir.path().join(".glitchlab");
        std::fs::create_dir_all(&glitchlab_dir).unwrap();
        let config = "workspace:\n  setup_command: \"false\"\n";
        std::fs::write(glitchlab_dir.join("config.yaml"), config).unwrap();

        let result = run_setup_command(dir.path()).await;
        assert!(result.is_err(), "should error on failing setup command");
    }
}
