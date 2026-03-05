use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// FlyOutput — command result
// ---------------------------------------------------------------------------

/// Result of a flyctl CLI invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlyOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

// ---------------------------------------------------------------------------
// FlyExecutor trait
// ---------------------------------------------------------------------------

/// Abstraction over `flyctl` CLI calls. Enables mock injection for tests.
pub trait FlyExecutor: Send + Sync {
    /// Check the status of a fly.io app.
    fn status<'a>(&'a self, app: &'a str) -> Pin<Box<dyn Future<Output = FlyOutput> + Send + 'a>>;

    /// Deploy a fly.io app from the given working directory.
    ///
    /// If `config_path` is provided, passes `--config <path>` to flyctl.
    fn deploy<'a>(
        &'a self,
        app: &'a str,
        working_dir: &'a Path,
        config_path: Option<&'a Path>,
    ) -> Pin<Box<dyn Future<Output = FlyOutput> + Send + 'a>>;

    /// Rollback a fly.io app to the previous release.
    fn rollback<'a>(&'a self, app: &'a str)
    -> Pin<Box<dyn Future<Output = FlyOutput> + Send + 'a>>;

    /// Create a new fly.io app. Uses `flyctl apps create <app> --yes`.
    fn create<'a>(
        &'a self,
        app: &'a str,
        org: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = FlyOutput> + Send + 'a>>;
}

// ---------------------------------------------------------------------------
// RealFlyExecutor — shells out to flyctl
// ---------------------------------------------------------------------------

/// Production executor that shells out to the `flyctl` binary.
pub struct RealFlyExecutor {
    flyctl_path: PathBuf,
}

impl RealFlyExecutor {
    /// Create with an explicit path to the flyctl binary.
    pub fn with_path(path: PathBuf) -> Self {
        Self { flyctl_path: path }
    }

    /// Create by searching common locations.
    ///
    /// Checks `~/.fly/bin/flyctl` first, then falls back to `flyctl` on PATH.
    pub fn new() -> Self {
        let home_path = dirs_path();
        if home_path.exists() {
            Self {
                flyctl_path: home_path,
            }
        } else {
            Self {
                flyctl_path: PathBuf::from("flyctl"),
            }
        }
    }

    async fn run_command(&self, args: &[&str], working_dir: Option<&Path>) -> FlyOutput {
        tracing::debug!(
            flyctl = %self.flyctl_path.display(),
            args = ?args,
            working_dir = ?working_dir,
            "running flyctl command"
        );
        let mut cmd = tokio::process::Command::new(&self.flyctl_path);
        cmd.args(args);
        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        match cmd.output().await {
            Ok(output) => FlyOutput {
                success: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(-1),
            },
            Err(e) => FlyOutput {
                success: false,
                stdout: String::new(),
                stderr: format!("failed to execute flyctl: {e}"),
                exit_code: -1,
            },
        }
    }
}

impl Default for RealFlyExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl FlyExecutor for RealFlyExecutor {
    fn status<'a>(&'a self, app: &'a str) -> Pin<Box<dyn Future<Output = FlyOutput> + Send + 'a>> {
        Box::pin(async move { self.run_command(&["status", "--app", app], None).await })
    }

    fn deploy<'a>(
        &'a self,
        app: &'a str,
        working_dir: &'a Path,
        config_path: Option<&'a Path>,
    ) -> Pin<Box<dyn Future<Output = FlyOutput> + Send + 'a>> {
        Box::pin(async move {
            let mut args = vec!["deploy", "--app", app];
            let config_str;
            if let Some(cp) = config_path {
                config_str = cp.to_string_lossy().into_owned();
                args.push("--config");
                args.push(&config_str);
            }
            self.run_command(&args, Some(working_dir)).await
        })
    }

    fn rollback<'a>(
        &'a self,
        app: &'a str,
    ) -> Pin<Box<dyn Future<Output = FlyOutput> + Send + 'a>> {
        Box::pin(async move {
            self.run_command(&["releases", "--app", app, "--image", "--json"], None)
                .await
        })
    }

    fn create<'a>(
        &'a self,
        app: &'a str,
        org: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = FlyOutput> + Send + 'a>> {
        Box::pin(async move {
            let mut args = vec!["apps", "create", app, "--yes"];
            if let Some(o) = org {
                args.push("-o");
                args.push(o);
            }
            self.run_command(&args, None).await
        })
    }
}

/// Default flyctl home path: `~/.fly/bin/flyctl`.
fn dirs_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".fly/bin/flyctl")
    } else {
        PathBuf::from("flyctl")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fly_output_construction() {
        let output = FlyOutput {
            success: true,
            stdout: "app is running".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        assert!(output.success);
        assert_eq!(output.exit_code, 0);
        assert!(output.stdout.contains("running"));
    }

    #[test]
    fn fly_output_failure() {
        let output = FlyOutput {
            success: false,
            stdout: String::new(),
            stderr: "not found".into(),
            exit_code: 1,
        };
        assert!(!output.success);
        assert_eq!(output.exit_code, 1);
    }

    #[test]
    fn fly_output_serde_roundtrip() {
        let output = FlyOutput {
            success: true,
            stdout: "ok".into(),
            stderr: String::new(),
            exit_code: 0,
        };
        let json = serde_json::to_string(&output).unwrap();
        let parsed: FlyOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.success, output.success);
        assert_eq!(parsed.stdout, output.stdout);
    }

    #[test]
    fn real_fly_executor_with_path() {
        let exec = RealFlyExecutor::with_path(PathBuf::from("/usr/bin/flyctl"));
        assert_eq!(exec.flyctl_path, PathBuf::from("/usr/bin/flyctl"));
    }

    #[test]
    fn real_fly_executor_default() {
        let exec = RealFlyExecutor::default();
        // Should resolve to either ~/.fly/bin/flyctl or just "flyctl"
        let path_str = exec.flyctl_path.to_string_lossy();
        assert!(
            path_str.contains("flyctl"),
            "path should contain flyctl: {path_str}"
        );
    }

    #[tokio::test]
    async fn real_fly_executor_status_missing_binary() {
        let exec =
            RealFlyExecutor::with_path(PathBuf::from("/nonexistent/flyctl-does-not-exist-12345"));
        let output = exec.status("test-app").await;
        assert!(!output.success);
        assert_eq!(output.exit_code, -1);
        assert!(output.stderr.contains("failed to execute"));
    }

    #[tokio::test]
    async fn real_fly_executor_deploy_missing_binary() {
        let exec =
            RealFlyExecutor::with_path(PathBuf::from("/nonexistent/flyctl-does-not-exist-12345"));
        let output = exec.deploy("test-app", Path::new("/tmp"), None).await;
        assert!(!output.success);
        assert_eq!(output.exit_code, -1);
    }

    #[tokio::test]
    async fn real_fly_executor_deploy_with_config_missing_binary() {
        let exec =
            RealFlyExecutor::with_path(PathBuf::from("/nonexistent/flyctl-does-not-exist-12345"));
        let output = exec
            .deploy("test-app", Path::new("/tmp"), Some(Path::new("fly.toml")))
            .await;
        assert!(!output.success);
        assert_eq!(output.exit_code, -1);
    }

    #[tokio::test]
    async fn real_fly_executor_rollback_missing_binary() {
        let exec =
            RealFlyExecutor::with_path(PathBuf::from("/nonexistent/flyctl-does-not-exist-12345"));
        let output = exec.rollback("test-app").await;
        assert!(!output.success);
        assert_eq!(output.exit_code, -1);
    }

    #[tokio::test]
    async fn real_fly_executor_create_missing_binary() {
        let exec =
            RealFlyExecutor::with_path(PathBuf::from("/nonexistent/flyctl-does-not-exist-12345"));
        let output = exec.create("test-app", None).await;
        assert!(!output.success);
        assert_eq!(output.exit_code, -1);
        assert!(output.stderr.contains("failed to execute"));
    }

    #[tokio::test]
    async fn real_fly_executor_create_with_org_missing_binary() {
        let exec =
            RealFlyExecutor::with_path(PathBuf::from("/nonexistent/flyctl-does-not-exist-12345"));
        let output = exec.create("test-app", Some("my-org")).await;
        assert!(!output.success);
        assert_eq!(output.exit_code, -1);
    }
}
