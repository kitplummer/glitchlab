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
