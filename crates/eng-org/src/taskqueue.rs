use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::info;

use glitchlab_kernel::error::{Error, Result};

// ---------------------------------------------------------------------------
// Task types
// ---------------------------------------------------------------------------

/// Status of a task in the backlog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Skipped,
}

/// A single task in the backlog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub objective: String,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default = "default_status")]
    pub status: TaskStatus,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
}

fn default_priority() -> u32 {
    100
}

fn default_status() -> TaskStatus {
    TaskStatus::Pending
}

// ---------------------------------------------------------------------------
// Backlog file format
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BacklogFile {
    tasks: Vec<Task>,
}

// ---------------------------------------------------------------------------
// TaskQueue
// ---------------------------------------------------------------------------

/// Manages a YAML-based task backlog.
///
/// Tasks are stored in a YAML file and picked in priority order (lowest number
/// first), respecting dependency constraints. Dependencies reference other task
/// IDs; a task is only eligible when all its dependencies are `Completed`.
pub struct TaskQueue {
    tasks: Vec<Task>,
    path: Option<std::path::PathBuf>,
}

impl TaskQueue {
    /// Load a task queue from a YAML file.
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("failed to read task file: {e}")))?;
        let backlog: BacklogFile = serde_yaml::from_str(&contents)
            .map_err(|e| Error::Config(format!("failed to parse task YAML: {e}")))?;
        Ok(Self {
            tasks: backlog.tasks,
            path: Some(path.to_path_buf()),
        })
    }

    /// Create a task queue from an in-memory list (for testing).
    pub fn from_tasks(tasks: Vec<Task>) -> Self {
        Self { tasks, path: None }
    }

    /// Pick the next eligible task: pending, all dependencies completed, lowest priority number.
    pub fn pick_next(&self) -> Option<&Task> {
        let completed_ids: std::collections::HashSet<&str> = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .map(|t| t.id.as_str())
            .collect();

        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Pending)
            .filter(|t| {
                t.depends_on
                    .iter()
                    .all(|dep| completed_ids.contains(dep.as_str()))
            })
            .min_by_key(|t| t.priority)
    }

    /// Update the status of a task by ID.
    pub fn update_status(&mut self, task_id: &str, status: TaskStatus) {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == task_id) {
            info!(task_id, ?status, "task status updated");
            task.status = status;
        }
    }

    /// Set the error message on a task.
    pub fn set_error(&mut self, task_id: &str, error: String) {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == task_id) {
            task.error = Some(error);
        }
    }

    /// Set the PR URL on a task.
    pub fn set_pr_url(&mut self, task_id: &str, url: String) {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == task_id) {
            task.pr_url = Some(url);
        }
    }

    /// Save the task queue back to disk (if loaded from a file).
    pub fn save(&self) -> Result<()> {
        let Some(ref path) = self.path else {
            return Ok(());
        };
        let backlog = BacklogFile {
            tasks: self.tasks.clone(),
        };
        let yaml = serde_yaml::to_string(&backlog)
            .map_err(|e| Error::Config(format!("failed to serialize tasks: {e}")))?;
        std::fs::write(path, yaml)
            .map_err(|e| Error::Config(format!("failed to write task file: {e}")))?;
        Ok(())
    }

    /// How many tasks are in each status.
    pub fn summary(&self) -> TaskQueueSummary {
        let mut s = TaskQueueSummary::default();
        for task in &self.tasks {
            match task.status {
                TaskStatus::Pending => s.pending += 1,
                TaskStatus::InProgress => s.in_progress += 1,
                TaskStatus::Completed => s.completed += 1,
                TaskStatus::Failed => s.failed += 1,
                TaskStatus::Skipped => s.skipped += 1,
            }
        }
        s.total = self.tasks.len() as u32;
        s
    }

    /// Inject new tasks into the queue (e.g. from decomposition).
    /// Tasks are appended and will be picked based on their priority.
    pub fn inject_tasks(&mut self, tasks: Vec<Task>) {
        let count = tasks.len();
        let ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        info!(?ids, count, "injecting sub-tasks into queue");
        self.tasks.extend(tasks);
    }

    /// All tasks (read-only).
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// Count of remaining actionable tasks (pending with satisfied deps).
    pub fn actionable_count(&self) -> usize {
        let completed_ids: std::collections::HashSet<&str> = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .map(|t| t.id.as_str())
            .collect();

        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Pending)
            .filter(|t| {
                t.depends_on
                    .iter()
                    .all(|dep| completed_ids.contains(dep.as_str()))
            })
            .count()
    }
}

/// Summary of task queue state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskQueueSummary {
    pub total: u32,
    pub pending: u32,
    pub in_progress: u32,
    pub completed: u32,
    pub failed: u32,
    pub skipped: u32,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tasks() -> Vec<Task> {
        vec![
            Task {
                id: "task-1".into(),
                objective: "First task".into(),
                priority: 1,
                status: TaskStatus::Pending,
                depends_on: vec![],
                error: None,
                pr_url: None,
            },
            Task {
                id: "task-2".into(),
                objective: "Second task".into(),
                priority: 2,
                status: TaskStatus::Pending,
                depends_on: vec!["task-1".into()],
                error: None,
                pr_url: None,
            },
            Task {
                id: "task-3".into(),
                objective: "Third task (low priority)".into(),
                priority: 10,
                status: TaskStatus::Pending,
                depends_on: vec![],
                error: None,
                pr_url: None,
            },
        ]
    }

    #[test]
    fn pick_next_respects_priority() {
        let queue = TaskQueue::from_tasks(sample_tasks());
        // task-1 (priority 1) and task-3 (priority 10) are both eligible.
        // task-2 depends on task-1, so it's blocked.
        let next = queue.pick_next().unwrap();
        assert_eq!(next.id, "task-1");
    }

    #[test]
    fn pick_next_respects_dependencies() {
        let mut queue = TaskQueue::from_tasks(sample_tasks());
        // Complete task-1, now task-2 should be eligible.
        queue.update_status("task-1", TaskStatus::Completed);
        let next = queue.pick_next().unwrap();
        assert_eq!(next.id, "task-2");
    }

    #[test]
    fn pick_next_returns_none_when_all_done() {
        let mut queue = TaskQueue::from_tasks(sample_tasks());
        queue.update_status("task-1", TaskStatus::Completed);
        queue.update_status("task-2", TaskStatus::Completed);
        queue.update_status("task-3", TaskStatus::Completed);
        assert!(queue.pick_next().is_none());
    }

    #[test]
    fn pick_next_skips_failed_and_in_progress() {
        let mut queue = TaskQueue::from_tasks(sample_tasks());
        queue.update_status("task-1", TaskStatus::Failed);
        queue.update_status("task-3", TaskStatus::InProgress);
        // Only task-2 is pending, but it depends on task-1 which failed (not completed).
        assert!(queue.pick_next().is_none());
    }

    #[test]
    fn pick_next_blocked_by_incomplete_dep() {
        let tasks = vec![
            Task {
                id: "a".into(),
                objective: "A".into(),
                priority: 1,
                status: TaskStatus::InProgress,
                depends_on: vec![],
                error: None,
                pr_url: None,
            },
            Task {
                id: "b".into(),
                objective: "B".into(),
                priority: 2,
                status: TaskStatus::Pending,
                depends_on: vec!["a".into()],
                error: None,
                pr_url: None,
            },
        ];
        let queue = TaskQueue::from_tasks(tasks);
        // "a" is in_progress, not completed — "b" is blocked.
        assert!(queue.pick_next().is_none());
    }

    #[test]
    fn update_status_sets_status() {
        let mut queue = TaskQueue::from_tasks(sample_tasks());
        queue.update_status("task-1", TaskStatus::Completed);
        let task = queue.tasks().iter().find(|t| t.id == "task-1").unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
    }

    #[test]
    fn set_error_and_pr_url() {
        let mut queue = TaskQueue::from_tasks(sample_tasks());
        queue.set_error("task-1", "something broke".into());
        queue.set_pr_url("task-1", "https://github.com/test/pr/1".into());
        let task = queue.tasks().iter().find(|t| t.id == "task-1").unwrap();
        assert_eq!(task.error.as_deref(), Some("something broke"));
        assert_eq!(task.pr_url.as_deref(), Some("https://github.com/test/pr/1"));
    }

    #[test]
    fn summary_counts() {
        let mut queue = TaskQueue::from_tasks(sample_tasks());
        queue.update_status("task-1", TaskStatus::Completed);
        queue.update_status("task-3", TaskStatus::Failed);
        let s = queue.summary();
        assert_eq!(s.total, 3);
        assert_eq!(s.pending, 1);
        assert_eq!(s.completed, 1);
        assert_eq!(s.failed, 1);
        assert_eq!(s.in_progress, 0);
        assert_eq!(s.skipped, 0);
    }

    #[test]
    fn actionable_count() {
        let queue = TaskQueue::from_tasks(sample_tasks());
        // task-1 and task-3 are actionable. task-2 is blocked.
        assert_eq!(queue.actionable_count(), 2);
    }

    #[test]
    fn actionable_count_after_completion() {
        let mut queue = TaskQueue::from_tasks(sample_tasks());
        queue.update_status("task-1", TaskStatus::Completed);
        // task-2 (unblocked) and task-3 are actionable.
        assert_eq!(queue.actionable_count(), 2);
    }

    #[test]
    fn load_and_save_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backlog.yaml");

        let yaml = r#"tasks:
  - id: "t1"
    objective: "Do the thing"
    priority: 1
    status: pending
  - id: "t2"
    objective: "Then this"
    priority: 2
    status: pending
    depends_on:
      - "t1"
"#;
        std::fs::write(&path, yaml).unwrap();

        let mut queue = TaskQueue::load(&path).unwrap();
        assert_eq!(queue.tasks().len(), 2);
        assert_eq!(queue.pick_next().unwrap().id, "t1");

        // Complete t1, save, reload.
        queue.update_status("t1", TaskStatus::Completed);
        queue.save().unwrap();

        let reloaded = TaskQueue::load(&path).unwrap();
        assert_eq!(reloaded.pick_next().unwrap().id, "t2");
    }

    #[test]
    fn load_minimal_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backlog.yaml");

        let yaml = "tasks:\n  - id: simple\n    objective: do it\n";
        std::fs::write(&path, yaml).unwrap();

        let queue = TaskQueue::load(&path).unwrap();
        assert_eq!(queue.tasks().len(), 1);
        let task = &queue.tasks()[0];
        assert_eq!(task.priority, 100); // default
        assert_eq!(task.status, TaskStatus::Pending); // default
        assert!(task.depends_on.is_empty()); // default
    }

    #[test]
    fn load_nonexistent_file_errors() {
        let result = TaskQueue::load(Path::new("/nonexistent/backlog.yaml"));
        assert!(result.is_err());
    }

    #[test]
    fn save_in_memory_queue_is_noop() {
        let queue = TaskQueue::from_tasks(sample_tasks());
        queue.save().unwrap(); // Should not error.
    }

    #[test]
    fn task_status_serde_roundtrip() {
        let statuses = vec![
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Skipped,
        ];
        for status in statuses {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: TaskStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn task_serde_yaml_roundtrip() {
        let task = Task {
            id: "test-1".into(),
            objective: "Test task".into(),
            priority: 5,
            status: TaskStatus::Completed,
            depends_on: vec!["dep-1".into()],
            error: Some("oops".into()),
            pr_url: Some("https://example.com/pr/1".into()),
        };
        let yaml = serde_yaml::to_string(&task).unwrap();
        let parsed: Task = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.id, task.id);
        assert_eq!(parsed.status, task.status);
        assert_eq!(parsed.depends_on, task.depends_on);
        assert_eq!(parsed.error, task.error);
        assert_eq!(parsed.pr_url, task.pr_url);
    }

    #[test]
    fn multiple_deps_all_must_complete() {
        let tasks = vec![
            Task {
                id: "a".into(),
                objective: "A".into(),
                priority: 1,
                status: TaskStatus::Completed,
                depends_on: vec![],
                error: None,
                pr_url: None,
            },
            Task {
                id: "b".into(),
                objective: "B".into(),
                priority: 1,
                status: TaskStatus::Pending,
                depends_on: vec![],
                error: None,
                pr_url: None,
            },
            Task {
                id: "c".into(),
                objective: "C".into(),
                priority: 1,
                status: TaskStatus::Pending,
                depends_on: vec!["a".into(), "b".into()],
                error: None,
                pr_url: None,
            },
        ];
        let queue = TaskQueue::from_tasks(tasks);
        // "c" depends on both "a" (completed) and "b" (pending).
        // Only "b" is actionable.
        let next = queue.pick_next().unwrap();
        assert_eq!(next.id, "b");
    }

    #[test]
    fn inject_tasks_adds_to_queue() {
        let mut queue = TaskQueue::from_tasks(sample_tasks());
        assert_eq!(queue.tasks().len(), 3);

        let new_tasks = vec![Task {
            id: "injected-1".into(),
            objective: "Injected task".into(),
            priority: 0, // highest priority
            status: TaskStatus::Pending,
            depends_on: vec![],
            error: None,
            pr_url: None,
        }];
        queue.inject_tasks(new_tasks);
        assert_eq!(queue.tasks().len(), 4);
        // Injected task has priority 0, so it should be picked first.
        let next = queue.pick_next().unwrap();
        assert_eq!(next.id, "injected-1");
    }
}
