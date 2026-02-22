use std::path::Path;

use anyhow::Result;
use glitchlab_eng_org::config::{EngConfig, check_api_keys, detect_test_command};

pub async fn execute(repo: Option<&Path>) -> Result<()> {
    println!("GLITCHLAB Status\n");

    // API keys.
    println!("API Keys:");
    for (key, available) in check_api_keys() {
        let status = if available { "available" } else { "missing" };
        println!("  {key}: {status}");
    }
    println!();

    // Config.
    let config = EngConfig::load(repo)?;
    println!("Routing:");
    println!("  planner:      {}", config.routing.planner);
    println!("  implementer:  {}", config.routing.implementer);
    println!("  debugger:     {}", config.routing.debugger);
    println!("  security:     {}", config.routing.security);
    println!("  release:      {}", config.routing.release);
    println!("  archivist:    {}", config.routing.archivist);
    println!();

    println!("Limits:");
    println!("  max fix attempts:   {}", config.limits.max_fix_attempts);
    println!(
        "  max tokens/task:    {}",
        config.limits.max_tokens_per_task
    );
    println!(
        "  max dollars/task:   ${:.2}",
        config.limits.max_dollars_per_task
    );
    println!();

    // Repo-specific info.
    if let Some(repo_path) = repo {
        if let Some(test_cmd) = detect_test_command(repo_path) {
            println!("Detected test command: {test_cmd}");
        } else {
            println!("No test command detected");
        }

        let glitchlab_dir = repo_path.join(".glitchlab");
        if glitchlab_dir.exists() {
            println!("GLITCHLAB initialized: yes");
        } else {
            println!("GLITCHLAB initialized: no (run `glitchlab init`)");
        }

        if !config.boundaries.protected_paths.is_empty() {
            println!(
                "Protected paths: {}",
                config.boundaries.protected_paths.join(", ")
            );
        }
    }

    Ok(())
}
