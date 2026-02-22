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
}
