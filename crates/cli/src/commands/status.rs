use std::path::Path;

use anyhow::Result;
use glitchlab_eng_org::config::{EngConfig, check_api_keys, detect_test_command};
use glitchlab_memory::{
    beads::BeadsClient,
    history::{HistoryBackend, JsonlHistory},
};

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

    // Memory backend information
    println!();
    let memory_info = get_memory_backend_info(repo).await;
    println!("Memory: {}", memory_info);

    Ok(())
}

async fn get_memory_backend_info(repo: Option<&Path>) -> String {
    let repo_path = repo.unwrap_or_else(|| Path::new("."));

    // Check individual backends
    let mut available = Vec::new();
    let mut unavailable = Vec::new();

    // Check Dolt
    let dolt_available = if let Ok(conn) = std::env::var("DOLT_DATABASE_URL") {
        match glitchlab_memory::dolt::DoltHistory::new(&conn).await {
            Ok(dh) => dh.is_available().await,
            Err(_) => false,
        }
    } else {
        false
    };

    if dolt_available {
        available.push("dolt");
    } else {
        unavailable.push("dolt");
    }

    // Check Beads
    let beads = BeadsClient::new(repo_path, None);
    if beads.is_available().await {
        available.push("beads");
    } else {
        unavailable.push("beads");
    }

    // JSONL is always available
    let jsonl = JsonlHistory::new(repo_path);
    if jsonl.is_available().await {
        available.push("jsonl");
    }

    // Format output
    let mut parts = Vec::new();
    if !available.is_empty() {
        parts.push(available.join(", "));
    }
    if !unavailable.is_empty() {
        let unavailable_str = unavailable
            .iter()
            .map(|name| format!("{} unavailable", name))
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("({})", unavailable_str));
    }

    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_memory_backend_info_format() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();

        let info = get_memory_backend_info(Some(repo_path)).await;

        println!("Actual output: '{}'", info);

        // Should contain jsonl as available
        assert!(info.contains("jsonl"));

        // Should contain dolt as unavailable (since DOLT_DATABASE_URL is not set in test)
        assert!(info.contains("dolt unavailable"));

        // Should match expected format with parentheses for unavailable backends
        assert!(info.contains("("));
        assert!(info.contains(")"));

        // Should have the general format: "available_backends (unavailable_backends)"
        let parts: Vec<&str> = info.split(" (").collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[1].ends_with(")"));
    }
}
