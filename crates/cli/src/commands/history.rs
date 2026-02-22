use std::path::Path;

use anyhow::Result;
use glitchlab_memory::history::TaskHistory;

pub async fn execute(repo: &Path, count: usize, stats: bool) -> Result<()> {
    let history = TaskHistory::new(repo);

    if stats {
        let s = history.get_stats()?;
        println!("GLITCHLAB History Stats\n");
        println!("  Total runs:    {}", s.total_runs);
        println!("  Successes:     {}", s.successes);
        println!("  Failures:      {}", s.failures);
        println!("  Total cost:    ${:.4}", s.total_cost);
        println!("  Total tokens:  {}", s.total_tokens);
        return Ok(());
    }

    let entries = history.get_recent(count)?;
    if entries.is_empty() {
        println!("No task history found.");
        return Ok(());
    }

    println!("GLITCHLAB History (last {count})\n");
    for entry in &entries {
        let cost = format!("${:.4}", entry.budget.estimated_cost);
        let tokens = entry.budget.total_tokens;
        print!("  {} | {} | {cost} | {tokens} tokens", entry.task_id, entry.status);
        if let Some(url) = &entry.pr_url {
            print!(" | {url}");
        }
        if let Some(err) = &entry.error {
            print!(" | error: {err}");
        }
        println!();
    }

    Ok(())
}
