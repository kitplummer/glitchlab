use std::path::Path;
use tempfile::TempDir;
use tokio::process::Command;

/// Get the path to the built `glitchlab` binary.
fn glitchlab_bin() -> String {
    env!("CARGO_BIN_EXE_glitchlab").to_string()
}

/// Initialize a test git repository.
fn init_test_repo(dir: &Path) -> String {
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::fs::write(dir.join("README.md"), "# Test").unwrap();
    std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(dir)
        .output()
        .unwrap();

    let output = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Create a mock backlog.yaml file with test tasks.
fn create_mock_backlog(dir: &Path) {
    let glitchlab_dir = dir.join(".glitchlab");
    let tasks_dir = glitchlab_dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    let backlog_content = r#"tasks:
  - id: "task-1"
    objective: "Add a new function to calculate the fibonacci sequence up to n terms"
    priority: 1
    status: pending
  - id: "task-2"
    objective: "Implement error handling for invalid input parameters in the main module"
    priority: 2
    status: pending
    depends_on:
      - "task-1"
  - id: "task-3"
    objective: "Write comprehensive unit tests for all public functions and edge cases"
    priority: 3
    status: pending
  - id: "task-4"
    objective: "This is a very long task objective that should be truncated when displayed in dry-run mode because it exceeds the 80 character limit that we want to enforce for readability"
    priority: 4
    status: pending
"#;

    std::fs::write(tasks_dir.join("backlog.yaml"), backlog_content).unwrap();
}

/// Create a minimal glitchlab config.
fn create_minimal_config(dir: &Path) {
    let glitchlab_dir = dir.join(".glitchlab");
    std::fs::create_dir_all(&glitchlab_dir).unwrap();

    let config_content = r#"
routing:
  planner: anthropic:claude-3-5-sonnet-20241022
  implementer: anthropic:claude-3-5-sonnet-20241022
  debugger: anthropic:claude-3-5-sonnet-20241022

limits:
  max_tokens_per_task: 100000
  max_dollars_per_task: 5.0

providers:
  anthropic:
    model_overrides: {}

memory:
  backend: file

intervention:
  pause_after_plan: true
  pause_before_pr: true
  pause_on_core_change: true
  pause_on_budget_exceeded: true
"#;

    std::fs::write(glitchlab_dir.join("config.yaml"), config_content).unwrap();
}

#[tokio::test]
async fn batch_dry_run_shows_task_info() {
    let dir = TempDir::new().unwrap();
    let _base_branch = init_test_repo(dir.path());

    // Create glitchlab config and mock backlog
    create_minimal_config(dir.path());
    create_mock_backlog(dir.path());

    // Commit the .glitchlab directory
    std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "add glitchlab config and tasks"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    // Run batch command with --dry-run
    let output = Command::new(glitchlab_bin())
        .args([
            "batch",
            "--repo",
            dir.path().to_str().unwrap(),
            "--budget",
            "20.0",
            "--dry-run",
        ])
        .output()
        .await
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined_output = format!("{stderr}{stdout}");

    // Should succeed
    assert!(
        output.status.success(),
        "batch --dry-run failed (exit {:?}):\nstderr: {stderr}\nstdout: {stdout}",
        output.status.code()
    );

    // Should show task queue summary
    assert!(
        combined_output.contains("task queue:") && combined_output.contains("pending"),
        "should show task queue summary:\n{combined_output}"
    );

    // Should show dry-run mode indication
    assert!(
        combined_output.contains("DRY RUN")
            || combined_output.contains("dry-run")
            || combined_output.contains("dry run"),
        "should indicate dry-run mode:\n{combined_output}"
    );

    // Should show task IDs
    assert!(
        combined_output.contains("task-1"),
        "should show task-1 ID:\n{combined_output}"
    );
    assert!(
        combined_output.contains("task-2"),
        "should show task-2 ID:\n{combined_output}"
    );

    // Should show objective summaries (first 80 chars)
    assert!(
        combined_output.contains("Add a new function to calculate"),
        "should show task-1 objective:\n{combined_output}"
    );
    assert!(
        combined_output.contains("Implement error handling"),
        "should show task-2 objective:\n{combined_output}"
    );

    // Should show budget information
    assert!(
        combined_output.contains("$5.0") || combined_output.contains("5.0"),
        "should show budget per task:\n{combined_output}"
    );

    // Should truncate long objectives to 80 chars
    let long_objective_line = combined_output
        .lines()
        .find(|line| line.contains("task-4"))
        .expect("should find task-4 line");

    // The objective part should be truncated
    assert!(
        long_objective_line
            .contains("This is a very long task objective that should be truncated when displayed"),
        "should show truncated objective for task-4:\n{long_objective_line}"
    );

    // Should NOT execute any tasks (no orchestrator messages)
    assert!(
        !combined_output.contains("starting orchestrator"),
        "should not start orchestrator in dry-run mode:\n{combined_output}"
    );
    assert!(
        !combined_output.contains("Pipeline Result"),
        "should not show pipeline results in dry-run mode:\n{combined_output}"
    );
}

#[tokio::test]
async fn batch_dry_run_with_no_pending_tasks() {
    let dir = TempDir::new().unwrap();
    let _base_branch = init_test_repo(dir.path());

    create_minimal_config(dir.path());

    // Create backlog with no pending tasks
    let glitchlab_dir = dir.path().join(".glitchlab");
    let tasks_dir = glitchlab_dir.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    let backlog_content = r#"tasks:
  - id: "completed-task"
    objective: "This task is already done"
    priority: 1
    status: completed
"#;

    std::fs::write(tasks_dir.join("backlog.yaml"), backlog_content).unwrap();

    // Commit the .glitchlab directory
    std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "add glitchlab config and completed task"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    // Run batch command with --dry-run
    let output = Command::new(glitchlab_bin())
        .args([
            "batch",
            "--repo",
            dir.path().to_str().unwrap(),
            "--budget",
            "10.0",
            "--dry-run",
        ])
        .output()
        .await
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined_output = format!("{stderr}{stdout}");

    // Should succeed
    assert!(
        output.status.success(),
        "batch --dry-run failed with no pending tasks:\n{combined_output}"
    );

    // Should show "no pending tasks" message
    assert!(
        combined_output.contains("no pending tasks") || combined_output.contains("0 pending"),
        "should indicate no pending tasks:\n{combined_output}"
    );
}

#[tokio::test]
async fn batch_dry_run_with_custom_tasks_file() {
    let dir = TempDir::new().unwrap();
    let _base_branch = init_test_repo(dir.path());

    create_minimal_config(dir.path());

    // Create custom tasks file
    let custom_tasks_content = r#"tasks:
  - id: "custom-task"
    objective: "Custom task from external file"
    priority: 1
    status: pending
"#;

    let custom_tasks_path = dir.path().join("my_tasks.yaml");
    std::fs::write(&custom_tasks_path, custom_tasks_content).unwrap();

    // Commit everything
    std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "add custom tasks"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    // Run batch command with --dry-run and custom tasks file
    let output = Command::new(glitchlab_bin())
        .args([
            "batch",
            "--repo",
            dir.path().to_str().unwrap(),
            "--budget",
            "10.0",
            "--tasks-file",
            custom_tasks_path.to_str().unwrap(),
            "--dry-run",
        ])
        .output()
        .await
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined_output = format!("{stderr}{stdout}");

    // Should succeed
    assert!(
        output.status.success(),
        "batch --dry-run with custom tasks file failed:\n{combined_output}"
    );

    // Should show the custom task
    assert!(
        combined_output.contains("custom-task"),
        "should show custom task ID:\n{combined_output}"
    );
    assert!(
        combined_output.contains("Custom task from external file"),
        "should show custom task objective:\n{combined_output}"
    );
}
