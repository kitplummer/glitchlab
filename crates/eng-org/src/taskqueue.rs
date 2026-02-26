use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::info;

use glitchlab_kernel::error::{Error, Result};
use glitchlab_kernel::outcome::OutcomeContext;
use glitchlab_memory::beads::{Bead, BeadsClient};

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
    Deferred,
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Skipped => "skipped",
            TaskStatus::Deferred => "deferred",
        };
        write!(f, "{}", s)
    }
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
    /// Decomposition depth: 0 for root tasks, incremented per decomposition level.
    #[serde(default)]
    pub decomposition_depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_context: Option<OutcomeContext>,
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

    /// Load a task queue from Beads via `bd list --json`.
    ///
    /// Only includes actionable issue types (task, bug, feature, chore).
    pub async fn load_from_beads(client: &BeadsClient) -> Result<Self> {
        let beads = client
            .list_parsed()
            .await
            .map_err(|e| Error::Config(format!("failed to load from beads: {e}")))?;
        let tasks: Vec<Task> = beads
            .into_iter()
            .filter(|b| is_actionable_type(&b.issue_type) && b.status != "closed")
            .map(bead_to_task)
            .collect();
        Ok(Self { tasks, path: None })
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
            .filter(|t| matches!(t.status, TaskStatus::Pending | TaskStatus::Deferred))
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
                TaskStatus::Deferred => s.deferred += 1,
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

    /// Check whether any pending or in-progress task has an ID starting with the given prefix.
    pub fn has_pending_with_prefix(&self, prefix: &str) -> bool {
        self.tasks.iter().any(|t| {
            matches!(t.status, TaskStatus::Pending | TaskStatus::InProgress)
                && t.id.starts_with(prefix)
        })
    }

    /// Total number of tasks in the queue (all statuses).
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Returns `true` if the queue contains no tasks.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// All tasks (read-only).
    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// Set the outcome context on a task (for deferred/failed tasks).
    pub fn set_outcome_context(&mut self, task_id: &str, ctx: OutcomeContext) {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == task_id) {
            task.outcome_context = Some(ctx);
        }
    }

    /// Get the outcome context for a task.
    pub fn outcome_context(&self, task_id: &str) -> Option<&OutcomeContext> {
        self.tasks
            .iter()
            .find(|t| t.id == task_id)
            .and_then(|t| t.outcome_context.as_ref())
    }

    /// Count of remaining actionable tasks (pending or deferred with satisfied deps).
    pub fn actionable_count(&self) -> usize {
        let completed_ids: std::collections::HashSet<&str> = self
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .map(|t| t.id.as_str())
            .collect();

        self.tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Pending | TaskStatus::Deferred))
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
    pub deferred: u32,
}

// ---------------------------------------------------------------------------
// Bead → Task conversion
// ---------------------------------------------------------------------------

/// Issue types we consider actionable for the task queue.
fn is_actionable_type(issue_type: &str) -> bool {
    matches!(
        issue_type,
        "task" | "bug" | "feature" | "chore" | "" // empty = untyped, include by default
    )
}

/// Convert a [`Bead`] into a [`Task`].
fn bead_to_task(bead: Bead) -> Task {
    let objective = if bead.description.is_empty() {
        bead.title
    } else {
        format!("{}\n\n{}", bead.title, bead.description)
    };

    let status = match bead.status.as_str() {
        "open" => TaskStatus::Pending,
        "in_progress" => TaskStatus::InProgress,
        "closed" => TaskStatus::Completed,
        "blocked" => TaskStatus::Skipped,
        "deferred" => TaskStatus::Deferred,
        _ => TaskStatus::Pending,
    };

    // Map beads priority 0-4 → task priority 0-100 (multiply by 25).
    let priority = (bead.priority.clamp(0, 4) as u32) * 25;

    let depends_on: Vec<String> = bead
        .dependencies
        .into_iter()
        .filter(|d| d.dep_type == "blocks")
        .map(|d| d.target_id)
        .collect();

    let pr_url = bead.external_ref.filter(|r| r.starts_with("http"));

    Task {
        id: bead.id,
        objective,
        priority,
        status,
        depends_on,
        decomposition_depth: 0,
        error: None,
        pr_url,
        outcome_context: None,
    }
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
                decomposition_depth: 0,
                error: None,
                pr_url: None,
                outcome_context: None,
            },
            Task {
                id: "task-2".into(),
                objective: "Second task".into(),
                priority: 2,
                status: TaskStatus::Pending,
                depends_on: vec!["task-1".into()],
                decomposition_depth: 0,
                error: None,
                pr_url: None,
                outcome_context: None,
            },
            Task {
                id: "task-3".into(),
                objective: "Third task (low priority)".into(),
                priority: 10,
                status: TaskStatus::Pending,
                depends_on: vec![],
                decomposition_depth: 0,
                error: None,
                pr_url: None,
                outcome_context: None,
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
                decomposition_depth: 0,
                error: None,
                pr_url: None,
                outcome_context: None,
            },
            Task {
                id: "b".into(),
                objective: "B".into(),
                priority: 2,
                status: TaskStatus::Pending,
                depends_on: vec!["a".into()],
                decomposition_depth: 0,
                error: None,
                pr_url: None,
                outcome_context: None,
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
            TaskStatus::Deferred,
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
            decomposition_depth: 0,
            error: Some("oops".into()),
            pr_url: Some("https://example.com/pr/1".into()),
            outcome_context: None,
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
                decomposition_depth: 0,
                error: None,
                pr_url: None,
                outcome_context: None,
            },
            Task {
                id: "b".into(),
                objective: "B".into(),
                priority: 1,
                status: TaskStatus::Pending,
                depends_on: vec![],
                decomposition_depth: 0,
                error: None,
                pr_url: None,
                outcome_context: None,
            },
            Task {
                id: "c".into(),
                objective: "C".into(),
                priority: 1,
                status: TaskStatus::Pending,
                depends_on: vec!["a".into(), "b".into()],
                decomposition_depth: 0,
                error: None,
                pr_url: None,
                outcome_context: None,
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
            decomposition_depth: 0,
            error: None,
            pr_url: None,
            outcome_context: None,
        }];
        queue.inject_tasks(new_tasks);
        assert_eq!(queue.tasks().len(), 4);
        // Injected task has priority 0, so it should be picked first.
        let next = queue.pick_next().unwrap();
        assert_eq!(next.id, "injected-1");
    }

    // -----------------------------------------------------------------------
    // Bead → Task conversion tests
    // -----------------------------------------------------------------------

    fn sample_bead() -> Bead {
        Bead {
            id: "bead-1".into(),
            title: "Implement feature X".into(),
            description: "Detailed description here".into(),
            status: "open".into(),
            priority: 1,
            issue_type: "task".into(),
            dependencies: vec![
                glitchlab_memory::beads::BeadDependency {
                    target_id: "bead-0".into(),
                    dep_type: "blocks".into(),
                },
                glitchlab_memory::beads::BeadDependency {
                    target_id: "bead-parent".into(),
                    dep_type: "parent".into(),
                },
            ],
            external_ref: Some("https://github.com/org/repo/pull/42".into()),
            labels: vec!["backend".into()],
            assignee: Some("alice".into()),
        }
    }

    #[test]
    fn bead_to_task_full() {
        let task = bead_to_task(sample_bead());
        assert_eq!(task.id, "bead-1");
        assert_eq!(
            task.objective,
            "Implement feature X\n\nDetailed description here"
        );
        assert_eq!(task.priority, 25); // 1 * 25
        assert_eq!(task.status, TaskStatus::Pending);
        // Only "blocks" deps, not "parent"
        assert_eq!(task.depends_on, vec!["bead-0"]);
        assert_eq!(
            task.pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/42")
        );
        assert!(task.error.is_none());
    }

    #[test]
    fn bead_to_task_no_description() {
        let bead = Bead {
            id: "b".into(),
            title: "Title only".into(),
            description: String::new(),
            status: "open".into(),
            priority: 0,
            issue_type: "bug".into(),
            dependencies: vec![],
            external_ref: None,
            labels: vec![],
            assignee: None,
        };
        let task = bead_to_task(bead);
        assert_eq!(task.objective, "Title only");
        assert_eq!(task.priority, 0); // 0 * 25
    }

    #[test]
    fn bead_to_task_status_mapping() {
        let statuses = [
            ("open", TaskStatus::Pending),
            ("in_progress", TaskStatus::InProgress),
            ("closed", TaskStatus::Completed),
            ("blocked", TaskStatus::Skipped),
            ("deferred", TaskStatus::Deferred),
            ("unknown_status", TaskStatus::Pending),
        ];
        for (bead_status, expected) in statuses {
            let bead = Bead {
                id: "s".into(),
                title: "S".into(),
                description: String::new(),
                status: bead_status.into(),
                priority: 2,
                issue_type: "task".into(),
                dependencies: vec![],
                external_ref: None,
                labels: vec![],
                assignee: None,
            };
            let task = bead_to_task(bead);
            assert_eq!(task.status, expected, "status mapping for '{bead_status}'");
        }
    }

    #[test]
    fn bead_to_task_priority_clamped() {
        // Priority > 4 should be clamped to 4 → 100
        let bead = Bead {
            id: "p".into(),
            title: "P".into(),
            description: String::new(),
            status: "open".into(),
            priority: 99,
            issue_type: "task".into(),
            dependencies: vec![],
            external_ref: None,
            labels: vec![],
            assignee: None,
        };
        let task = bead_to_task(bead);
        assert_eq!(task.priority, 100);

        // Negative priority should be clamped to 0 → 0
        let bead_neg = Bead {
            id: "n".into(),
            title: "N".into(),
            description: String::new(),
            status: "open".into(),
            priority: -5,
            issue_type: "task".into(),
            dependencies: vec![],
            external_ref: None,
            labels: vec![],
            assignee: None,
        };
        let task_neg = bead_to_task(bead_neg);
        assert_eq!(task_neg.priority, 0);
    }

    #[test]
    fn bead_to_task_external_ref_not_url() {
        let bead = Bead {
            id: "e".into(),
            title: "E".into(),
            description: String::new(),
            status: "open".into(),
            priority: 2,
            issue_type: "task".into(),
            dependencies: vec![],
            external_ref: Some("JIRA-1234".into()),
            labels: vec![],
            assignee: None,
        };
        let task = bead_to_task(bead);
        assert!(
            task.pr_url.is_none(),
            "non-URL external_ref should not become pr_url"
        );
    }

    #[test]
    fn is_actionable_type_filters() {
        assert!(is_actionable_type("task"));
        assert!(is_actionable_type("bug"));
        assert!(is_actionable_type("feature"));
        assert!(is_actionable_type("chore"));
        assert!(is_actionable_type("")); // untyped
        assert!(!is_actionable_type("epic"));
        assert!(!is_actionable_type("milestone"));
    }

    #[tokio::test]
    async fn load_from_beads_filters_and_converts() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"[
            {"id":"t1","title":"A task","issue_type":"task","priority":1},
            {"id":"t2","title":"An epic","issue_type":"epic","priority":0},
            {"id":"t3","title":"A bug","issue_type":"bug","priority":2,"status":"in_progress"},
            {"id":"t4","title":"Done task","issue_type":"task","priority":1,"status":"closed"}
        ]"#;
        let script = mock_bd_script(dir.path(), json);
        let client = BeadsClient::new(dir.path(), Some(script.to_string_lossy().into()));

        let queue = TaskQueue::load_from_beads(&client).await.unwrap();
        // "epic" and "closed" should be filtered out
        assert_eq!(queue.tasks().len(), 2);
        assert_eq!(queue.tasks()[0].id, "t1");
        assert_eq!(queue.tasks()[1].id, "t3");
        assert_eq!(queue.tasks()[1].status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn load_from_beads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let script = mock_bd_script(dir.path(), "[]");
        let client = BeadsClient::new(dir.path(), Some(script.to_string_lossy().into()));

        let queue = TaskQueue::load_from_beads(&client).await.unwrap();
        assert!(queue.tasks().is_empty());
    }

    #[tokio::test]
    async fn load_from_beads_error_propagates() {
        let client = BeadsClient::new(Path::new("/tmp"), Some("nonexistent-bd-binary-xyz".into()));
        let result = TaskQueue::load_from_beads(&client).await;
        assert!(result.is_err());
    }

    /// Create a temporary shell script that echoes the given JSON to stdout.
    fn mock_bd_script(dir: &std::path::Path, json: &str) -> std::path::PathBuf {
        let script = dir.join("mock_bd");
        let content = format!("#!/bin/sh\nprintf '%s' '{}'\n", json.replace('\'', "'\\''"));
        std::fs::write(&script, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        script
    }

    // -----------------------------------------------------------------------
    // Deferred + outcome_context tests
    // -----------------------------------------------------------------------

    #[test]
    fn deferred_tasks_are_picked() {
        let tasks = vec![
            Task {
                id: "d1".into(),
                objective: "Deferred task".into(),
                priority: 1,
                status: TaskStatus::Deferred,
                depends_on: vec![],
                decomposition_depth: 0,
                error: None,
                pr_url: None,
                outcome_context: None,
            },
            Task {
                id: "p1".into(),
                objective: "Pending task".into(),
                priority: 2,
                status: TaskStatus::Pending,
                depends_on: vec![],
                decomposition_depth: 0,
                error: None,
                pr_url: None,
                outcome_context: None,
            },
        ];
        let queue = TaskQueue::from_tasks(tasks);
        // Deferred d1 (priority 1) should be picked before p1 (priority 2).
        let next = queue.pick_next().unwrap();
        assert_eq!(next.id, "d1");
    }

    #[test]
    fn deferred_counted_in_actionable() {
        let tasks = vec![Task {
            id: "d1".into(),
            objective: "Deferred".into(),
            priority: 1,
            status: TaskStatus::Deferred,
            depends_on: vec![],
            decomposition_depth: 0,
            error: None,
            pr_url: None,
            outcome_context: None,
        }];
        let queue = TaskQueue::from_tasks(tasks);
        assert_eq!(queue.actionable_count(), 1);
    }

    #[test]
    fn deferred_counted_in_summary() {
        let tasks = vec![Task {
            id: "d1".into(),
            objective: "Deferred".into(),
            priority: 1,
            status: TaskStatus::Deferred,
            depends_on: vec![],
            decomposition_depth: 0,
            error: None,
            pr_url: None,
            outcome_context: None,
        }];
        let queue = TaskQueue::from_tasks(tasks);
        let s = queue.summary();
        assert_eq!(s.deferred, 1);
        assert_eq!(s.pending, 0);
    }

    #[test]
    fn set_and_get_outcome_context() {
        use glitchlab_kernel::outcome::{ObstacleKind, OutcomeContext};

        let mut queue = TaskQueue::from_tasks(sample_tasks());
        assert!(queue.outcome_context("task-1").is_none());

        let ctx = OutcomeContext {
            approach: "tried X".into(),
            obstacle: ObstacleKind::Unknown {
                detail: "failed".into(),
            },
            discoveries: vec!["found Y".into()],
            recommendation: None,
            files_explored: vec![],
        };
        queue.set_outcome_context("task-1", ctx);

        let stored = queue.outcome_context("task-1").unwrap();
        assert_eq!(stored.approach, "tried X");
        assert_eq!(stored.discoveries.len(), 1);
    }

    #[test]
    fn outcome_context_survives_serde() {
        use glitchlab_kernel::outcome::{ObstacleKind, OutcomeContext};

        let task = Task {
            id: "oc".into(),
            objective: "Test".into(),
            priority: 1,
            status: TaskStatus::Deferred,
            depends_on: vec![],
            decomposition_depth: 0,
            error: None,
            pr_url: None,
            outcome_context: Some(OutcomeContext {
                approach: "approach A".into(),
                obstacle: ObstacleKind::TestFailure {
                    attempts: 2,
                    last_error: "panic".into(),
                },
                discoveries: vec![],
                recommendation: Some("try B".into()),
                files_explored: vec!["a.rs".into()],
            }),
        };
        let json = serde_json::to_string(&task).unwrap();
        assert!(json.contains("outcome_context"));
        let parsed: Task = serde_json::from_str(&json).unwrap();
        let oc = parsed.outcome_context.unwrap();
        assert_eq!(oc.approach, "approach A");
        assert_eq!(oc.recommendation.as_deref(), Some("try B"));
    }

    #[test]
    fn task_without_outcome_context_backward_compat() {
        // Tasks serialised before outcome_context was added.
        let json = r#"{"id":"old","objective":"old task","priority":1,"status":"pending"}"#;
        let parsed: Task = serde_json::from_str(json).unwrap();
        assert!(parsed.outcome_context.is_none());
    }

    #[test]
    fn task_status_deferred_serde() {
        let status = TaskStatus::Deferred;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"deferred\"");
        let parsed: TaskStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, TaskStatus::Deferred);
    }

    #[test]
    fn task_status_display() {
        assert_eq!(TaskStatus::Pending.to_string(), "pending");
        assert_eq!(TaskStatus::InProgress.to_string(), "in_progress");
        assert_eq!(TaskStatus::Completed.to_string(), "completed");
        assert_eq!(TaskStatus::Failed.to_string(), "failed");
        assert_eq!(TaskStatus::Skipped.to_string(), "skipped");
        assert_eq!(TaskStatus::Deferred.to_string(), "deferred");
    }

    // -----------------------------------------------------------------------
    // Decomposition depth tests
    // -----------------------------------------------------------------------

    #[test]
    fn task_serde_with_decomposition_depth() {
        let task = Task {
            id: "depth-test".into(),
            objective: "Test depth".into(),
            priority: 1,
            status: TaskStatus::Pending,
            depends_on: vec![],
            decomposition_depth: 2,
            error: None,
            pr_url: None,
            outcome_context: None,
        };
        let json = serde_json::to_string(&task).unwrap();
        let parsed: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.decomposition_depth, 2);

        // YAML roundtrip too.
        let yaml = serde_yaml::to_string(&task).unwrap();
        let parsed_yaml: Task = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed_yaml.decomposition_depth, 2);
    }

    #[test]
    fn task_decomposition_depth_defaults_to_zero() {
        // Tasks serialised before decomposition_depth existed.
        let json = r#"{"id":"old","objective":"old task","priority":1,"status":"pending"}"#;
        let parsed: Task = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.decomposition_depth, 0);
    }

    // -----------------------------------------------------------------------
    // has_pending_with_prefix tests
    // -----------------------------------------------------------------------

    #[test]
    fn has_pending_with_prefix_finds_match() {
        let tasks = vec![Task {
            id: "tqm-stuck-agents-abc123".into(),
            objective: "fix stuck".into(),
            priority: 5,
            status: TaskStatus::Pending,
            depends_on: vec![],
            decomposition_depth: 0,
            error: None,
            pr_url: None,
            outcome_context: None,
        }];
        let queue = TaskQueue::from_tasks(tasks);
        assert!(queue.has_pending_with_prefix("tqm-stuck-agents"));
    }

    #[test]
    fn has_pending_with_prefix_no_match() {
        let tasks = vec![Task {
            id: "task-1".into(),
            objective: "regular task".into(),
            priority: 5,
            status: TaskStatus::Pending,
            depends_on: vec![],
            decomposition_depth: 0,
            error: None,
            pr_url: None,
            outcome_context: None,
        }];
        let queue = TaskQueue::from_tasks(tasks);
        assert!(!queue.has_pending_with_prefix("tqm-stuck-agents"));
    }

    #[test]
    fn has_pending_with_prefix_ignores_completed() {
        let tasks = vec![Task {
            id: "tqm-stuck-agents-abc123".into(),
            objective: "fix stuck".into(),
            priority: 5,
            status: TaskStatus::Completed,
            depends_on: vec![],
            decomposition_depth: 0,
            error: None,
            pr_url: None,
            outcome_context: None,
        }];
        let queue = TaskQueue::from_tasks(tasks);
        assert!(!queue.has_pending_with_prefix("tqm-stuck-agents"));
    }

    #[test]
    fn has_pending_with_prefix_ignores_failed() {
        let tasks = vec![Task {
            id: "tqm-stuck-agents-abc123".into(),
            objective: "fix stuck".into(),
            priority: 5,
            status: TaskStatus::Failed,
            depends_on: vec![],
            decomposition_depth: 0,
            error: None,
            pr_url: None,
            outcome_context: None,
        }];
        let queue = TaskQueue::from_tasks(tasks);
        assert!(!queue.has_pending_with_prefix("tqm-stuck-agents"));
    }
}
