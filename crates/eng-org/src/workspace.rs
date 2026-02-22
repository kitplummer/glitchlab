use std::path::{Path, PathBuf};
use std::process::Stdio;

use glitchlab_kernel::error::{Error, Result};
use tokio::process::Command;
use tracing::{info, warn};

/// Isolated git worktree for a single task.
///
/// Each task gets its own worktree and branch, ensuring no interference
/// with the main branch or other concurrent tasks.
pub struct Workspace {
    /// Path to the original repository.
    repo_path: PathBuf,
    /// Task identifier.
    task_id: String,
    /// Branch name: `glitchlab/{task_id}`.
    branch_name: String,
    /// Path to the created worktree.
    worktree_path: PathBuf,
    /// Whether the worktree has been created.
    created: bool,
}

impl Workspace {
    pub fn new(repo_path: &Path, task_id: &str, worktree_base: &str) -> Self {
        let repo_path = repo_path.to_path_buf();
        let branch_name = format!("glitchlab/{task_id}");
        let worktree_path = repo_path.join(worktree_base).join(task_id);

        Self {
            repo_path,
            task_id: task_id.into(),
            branch_name,
            worktree_path,
            created: false,
        }
    }

    pub fn repo_path(&self) -> &Path {
        &self.repo_path
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn branch_name(&self) -> &str {
        &self.branch_name
    }

    pub fn worktree_path(&self) -> &Path {
        &self.worktree_path
    }

    pub fn is_created(&self) -> bool {
        self.created
    }

    /// Create the isolated worktree and branch.
    pub async fn create(&mut self, base_branch: &str) -> Result<&Path> {
        // Remove any stale branch from a previous run with the same task_id.
        let _ = self.git_repo(&["branch", "-D", &self.branch_name]).await;

        // Create the branch from base.
        self.git_repo(&["branch", &self.branch_name, base_branch])
            .await
            .map_err(|e| Error::Workspace(format!("failed to create branch: {e}")))?;

        // Create the worktree.
        let wt = self.worktree_path.to_string_lossy().to_string();
        self.git_repo(&["worktree", "add", &wt, &self.branch_name])
            .await
            .map_err(|e| Error::Workspace(format!("failed to create worktree: {e}")))?;

        self.created = true;
        info!(
            task_id = %self.task_id,
            branch = %self.branch_name,
            path = %self.worktree_path.display(),
            "workspace created"
        );

        Ok(&self.worktree_path)
    }

    /// Stage all changes and commit.
    /// Returns the short commit SHA, or None if there was nothing to commit.
    pub async fn commit(&self, message: &str) -> Result<Option<String>> {
        // Stage everything.
        self.git_wt(&["add", "-A"]).await?;

        // Check if there's anything to commit.
        let status = self.git_wt_output(&["status", "--porcelain"]).await?;
        if status.trim().is_empty() {
            return Ok(None);
        }

        // Commit.
        self.git_wt(&["commit", "-m", message]).await?;

        // Get short SHA.
        let sha = self
            .git_wt_output(&["rev-parse", "--short", "HEAD"])
            .await?;

        Ok(Some(sha.trim().to_string()))
    }

    /// Push the branch to origin.
    pub async fn push(&self) -> Result<()> {
        self.git_wt(&["push", "-u", "origin", &self.branch_name])
            .await
            .map_err(|e| Error::Workspace(format!("failed to push: {e}")))
    }

    /// Get diff --stat against a base branch.
    pub async fn diff_stat(&self, base: &str) -> Result<String> {
        self.git_wt_output(&["diff", "--stat", base]).await
    }

    /// Get full diff against a base branch.
    pub async fn diff_full(&self, base: &str) -> Result<String> {
        self.git_wt_output(&["diff", base]).await
    }

    /// Remove the worktree and prune.
    pub async fn cleanup(&mut self) -> Result<()> {
        if !self.created {
            return Ok(());
        }

        let wt = self.worktree_path.to_string_lossy().to_string();

        // Try git worktree remove first.
        if let Err(e) = self.git_repo(&["worktree", "remove", &wt, "--force"]).await {
            warn!(error = %e, "git worktree remove failed, falling back to rm");
            if self.worktree_path.exists() {
                tokio::fs::remove_dir_all(&self.worktree_path)
                    .await
                    .map_err(|e| Error::Workspace(format!("failed to remove worktree dir: {e}")))?;
            }
        }

        // Prune stale worktrees.
        let _ = self.git_repo(&["worktree", "prune"]).await;

        // Delete the task branch so the next run with the same task_id
        // doesn't collide.
        if let Err(e) = self.git_repo(&["branch", "-D", &self.branch_name]).await {
            warn!(error = %e, "git branch delete failed (may already be gone)");
        }

        self.created = false;
        info!(task_id = %self.task_id, "workspace cleaned up");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Git helpers
    // -----------------------------------------------------------------------

    /// Run a git command in the repo root.
    async fn git_repo(&self, args: &[&str]) -> Result<()> {
        let path = self.repo_path.to_string_lossy();
        let mut cmd_args = vec!["-C", &*path];
        cmd_args.extend_from_slice(args);
        run_git(&cmd_args).await
    }

    /// Run a git command in the worktree.
    async fn git_wt(&self, args: &[&str]) -> Result<()> {
        let path = self.worktree_path.to_string_lossy();
        let mut cmd_args = vec!["-C", &*path];
        cmd_args.extend_from_slice(args);
        run_git(&cmd_args).await
    }

    /// Run a git command in the worktree and capture stdout.
    async fn git_wt_output(&self, args: &[&str]) -> Result<String> {
        let path = self.worktree_path.to_string_lossy();
        let mut cmd_args = vec!["-C", &*path];
        cmd_args.extend_from_slice(args);
        run_git_output(&cmd_args).await
    }
}

/// Run a git command, returning an error if it fails.
pub(crate) async fn run_git(args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(Error::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Workspace(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(())
}

/// Run a git command and capture stdout.
pub(crate) async fn run_git_output(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(Error::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Workspace(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_new() {
        let ws = Workspace::new(Path::new("/tmp/repo"), "task-123", ".glitchlab/worktrees");
        assert_eq!(ws.repo_path(), Path::new("/tmp/repo"));
        assert_eq!(ws.task_id(), "task-123");
        assert_eq!(ws.branch_name(), "glitchlab/task-123");
        assert_eq!(
            ws.worktree_path(),
            Path::new("/tmp/repo/.glitchlab/worktrees/task-123")
        );
        assert!(!ws.is_created());
    }

    #[tokio::test]
    async fn workspace_create_commit_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        // Initialize a git repo with a commit.
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("README.md"), "# Test").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        // Get the base branch name.
        let output = std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let base_branch = String::from_utf8_lossy(&output.stdout).trim().to_string();

        let mut ws = Workspace::new(dir.path(), "test-task", ".worktrees");

        // Create.
        let path = ws.create(&base_branch).await.unwrap();
        assert!(path.exists());
        assert!(ws.is_created());

        // Commit with no changes.
        let sha = ws.commit("test commit").await.unwrap();
        assert!(sha.is_none());

        // Create a file and commit.
        std::fs::write(ws.worktree_path().join("new_file.txt"), "hello").unwrap();
        let sha = ws.commit("add file").await.unwrap();
        assert!(sha.is_some());

        // Diff stat.
        let stat = ws.diff_stat(&base_branch).await.unwrap();
        assert!(stat.contains("new_file.txt"));

        // Diff full.
        let diff = ws.diff_full(&base_branch).await.unwrap();
        assert!(diff.contains("hello"));

        // Cleanup.
        ws.cleanup().await.unwrap();
        assert!(!ws.is_created());

        // Branch should be gone after cleanup.
        let branches = std::process::Command::new("git")
            .args(["-C", &dir.path().to_string_lossy(), "branch"])
            .output()
            .unwrap();
        let branch_list = String::from_utf8_lossy(&branches.stdout);
        assert!(
            !branch_list.contains("glitchlab/test-task"),
            "branch should be deleted after cleanup, got: {branch_list}"
        );
    }

    #[tokio::test]
    async fn workspace_create_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        // Initialize a git repo with a commit.
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("README.md"), "# Test").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let output = std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let base_branch = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // First run: create and cleanup (leaves no branch thanks to cleanup).
        let mut ws1 = Workspace::new(dir.path(), "reuse-task", ".worktrees");
        ws1.create(&base_branch).await.unwrap();
        ws1.cleanup().await.unwrap();

        // Second run: should succeed even though the branch was just used.
        let mut ws2 = Workspace::new(dir.path(), "reuse-task", ".worktrees");
        let path = ws2.create(&base_branch).await.unwrap();
        assert!(path.exists());
        ws2.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_noop_when_not_created() {
        let mut ws = Workspace::new(Path::new("/tmp/repo"), "task-456", ".worktrees");
        ws.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn run_git_failure() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_git(&["-C", &dir.path().to_string_lossy(), "log"]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn run_git_output_failure() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_git_output(&["-C", &dir.path().to_string_lossy(), "log"]).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn run_git_output_success() {
        let dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let result = run_git_output(&["-C", &dir.path().to_string_lossy(), "status"]).await;
        assert!(result.is_ok());
    }
}
