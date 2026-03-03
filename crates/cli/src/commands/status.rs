use std::path::Path;

use anyhow::Result;
use glitchlab_eng_org::config::{EngConfig, check_api_keys, detect_test_command};
use glitchlab_memory::{
    beads::BeadsClient,
    history::{HistoryBackend, HistoryQuery, JsonlHistory},
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

    // History summary from the best available backend
    println!();
    let (backend, backend_name) = select_history_backend(repo).await;
    let summary = get_history_summary(backend.as_ref(), &backend_name).await;
    print!("{summary}");

    Ok(())
}

/// Select the best available history backend: Dolt (if `DOLT_DATABASE_URL` is set and
/// reachable), otherwise JSONL.
async fn select_history_backend(repo: Option<&Path>) -> (Box<dyn HistoryBackend>, String) {
    let repo_path = repo.unwrap_or_else(|| Path::new("."));

    // Try Dolt when the connection string is provided.
    if let Ok(conn) = std::env::var("DOLT_DATABASE_URL")
        && let Ok(dh) = glitchlab_memory::dolt::DoltHistory::new(&conn).await
        && dh.is_available().await
    {
        return (Box::new(dh), "dolt".into());
    }

    // Fall back to JSONL.
    (Box::new(JsonlHistory::new(repo_path)), "jsonl".into())
}

/// Query `backend` for stats and recent entries, then format a human-readable summary.
async fn get_history_summary(backend: &dyn HistoryBackend, backend_name: &str) -> String {
    let mut out = String::new();

    // --- Stats block ---
    match backend.stats().await {
        Ok(stats) => {
            out.push_str(&format!("History (via {backend_name}):\n"));
            out.push_str(&format!("  Total tasks:  {}\n", stats.total_runs));
            if stats.total_runs > 0 {
                let pct = 100.0 * stats.successes as f64 / stats.total_runs as f64;
                out.push_str(&format!(
                    "  Successes:    {} ({pct:.0}%)\n",
                    stats.successes
                ));
                out.push_str(&format!("  Failures:     {}\n", stats.failures));
                out.push_str(&format!("  Total cost:   ${:.4}\n", stats.total_cost));
                out.push_str(&format!("  Total tokens: {}\n", stats.total_tokens));
            }
        }
        Err(e) => {
            out.push_str(&format!("History: error reading stats ({e})\n"));
            return out;
        }
    }

    // --- Recent tasks ---
    let q = HistoryQuery {
        limit: 5,
        ..Default::default()
    };
    match backend.query(&q).await {
        Ok(entries) if !entries.is_empty() => {
            out.push_str("\nRecent tasks:\n");
            for entry in &entries {
                let date = entry.timestamp.format("%Y-%m-%d %H:%M");
                let icon = if matches!(entry.status.as_str(), "pr_created" | "committed") {
                    '✓'
                } else {
                    '✗'
                };
                // Truncate long task IDs so the table stays readable.
                let task_id: String = entry.task_id.chars().take(48).collect();
                out.push_str(&format!(
                    "  [{date}] {icon} {task_id}  ({})\n",
                    entry.status
                ));
                if let Some(ref err) = entry.error {
                    let err_short: String = err.chars().take(72).collect();
                    out.push_str(&format!("            error: {err_short}\n"));
                }
            }
        }
        Ok(_) => {
            out.push_str("\nNo task history yet.\n");
        }
        Err(e) => {
            out.push_str(&format!("\nError reading recent tasks: {e}\n"));
        }
    }

    out
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
    use glitchlab_kernel::budget::BudgetSummary;
    use glitchlab_memory::history::{EventsSummary, HistoryEntry};
    use tempfile::TempDir;

    fn make_entry(task_id: &str, status: &str) -> HistoryEntry {
        HistoryEntry {
            timestamp: chrono::Utc::now(),
            task_id: task_id.into(),
            status: status.into(),
            pr_url: None,
            branch: None,
            error: if status == "error" {
                Some("something broke".into())
            } else {
                None
            },
            budget: BudgetSummary {
                total_tokens: 1000,
                estimated_cost: 0.10,
                call_count: 2,
                tokens_remaining: 149_000,
                dollars_remaining: 9.90,
            },
            events_summary: EventsSummary::default(),
            stage_outputs: None,
            events: None,
            outcome_context: None,
        }
    }

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

    #[tokio::test]
    async fn test_history_summary_empty_backend() {
        let temp_dir = TempDir::new().unwrap();
        let backend = JsonlHistory::new(temp_dir.path());

        let summary = get_history_summary(&backend, "jsonl").await;

        assert!(
            summary.contains("History (via jsonl)"),
            "should show backend name: {summary}"
        );
        assert!(
            summary.contains("Total tasks:  0"),
            "should show zero tasks: {summary}"
        );
        assert!(
            summary.contains("No task history yet"),
            "should indicate no history: {summary}"
        );
    }

    #[tokio::test]
    async fn test_history_summary_with_entries() {
        let temp_dir = TempDir::new().unwrap();
        let backend = JsonlHistory::new(temp_dir.path());

        backend
            .record(&make_entry("task-1", "pr_created"))
            .await
            .unwrap();
        backend
            .record(&make_entry("task-2", "error"))
            .await
            .unwrap();
        backend
            .record(&make_entry("task-3", "pr_created"))
            .await
            .unwrap();

        let summary = get_history_summary(&backend, "jsonl").await;

        assert!(
            summary.contains("History (via jsonl)"),
            "should show backend name: {summary}"
        );
        assert!(
            summary.contains("Total tasks:  3"),
            "should show 3 tasks: {summary}"
        );
        assert!(
            summary.contains("Successes:"),
            "should show successes: {summary}"
        );
        assert!(
            summary.contains("Failures:"),
            "should show failures: {summary}"
        );
        assert!(
            summary.contains("Recent tasks:"),
            "should show recent tasks header: {summary}"
        );
        assert!(
            summary.contains("task-3"),
            "should show most recent task: {summary}"
        );
    }

    #[tokio::test]
    async fn test_history_summary_shows_success_icon() {
        let temp_dir = TempDir::new().unwrap();
        let backend = JsonlHistory::new(temp_dir.path());

        backend
            .record(&make_entry("gl-success", "pr_created"))
            .await
            .unwrap();

        let summary = get_history_summary(&backend, "jsonl").await;

        assert!(
            summary.contains('✓'),
            "should show ✓ for success: {summary}"
        );
    }

    #[tokio::test]
    async fn test_history_summary_shows_failure_icon_and_error() {
        let temp_dir = TempDir::new().unwrap();
        let backend = JsonlHistory::new(temp_dir.path());

        backend
            .record(&make_entry("gl-fail", "error"))
            .await
            .unwrap();

        let summary = get_history_summary(&backend, "jsonl").await;

        assert!(
            summary.contains('✗'),
            "should show ✗ for failure: {summary}"
        );
        assert!(
            summary.contains("something broke"),
            "should show error message: {summary}"
        );
    }

    #[tokio::test]
    async fn test_select_history_backend_fallback_to_jsonl() {
        // Without DOLT_DATABASE_URL, should use jsonl
        let temp_dir = TempDir::new().unwrap();

        // Make sure env var is unset for this test.
        // SAFETY: single-threaded test; no other thread reads this variable concurrently.
        unsafe { std::env::remove_var("DOLT_DATABASE_URL") };

        let (backend, name): (Box<dyn HistoryBackend>, String) =
            select_history_backend(Some(temp_dir.path())).await;
        assert_eq!(
            name, "jsonl",
            "should fall back to jsonl without DOLT_DATABASE_URL"
        );
        assert!(backend.is_available().await);
    }

    #[tokio::test]
    async fn test_history_summary_cost_and_tokens() {
        let temp_dir = TempDir::new().unwrap();
        let backend = JsonlHistory::new(temp_dir.path());

        backend
            .record(&make_entry("t1", "pr_created"))
            .await
            .unwrap();

        let summary = get_history_summary(&backend, "jsonl").await;

        // Cost and token totals should appear
        assert!(
            summary.contains("Total cost:"),
            "should show total cost: {summary}"
        );
        assert!(
            summary.contains("Total tokens:"),
            "should show total tokens: {summary}"
        );
    }

    #[tokio::test]
    async fn test_history_summary_committed_shows_success_icon() {
        let temp_dir = TempDir::new().unwrap();
        let backend = JsonlHistory::new(temp_dir.path());

        backend
            .record(&make_entry("t1", "committed"))
            .await
            .unwrap();

        let summary = get_history_summary(&backend, "jsonl").await;
        assert!(summary.contains('✓'), "committed should show ✓: {summary}");
    }
}
