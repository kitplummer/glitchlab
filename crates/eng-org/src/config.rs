use std::collections::HashMap;
use std::path::Path;

use glitchlab_kernel::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// Full GLITCHLAB configuration (engineering org).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngConfig {
    pub routing: RoutingConfig,
    pub limits: LimitsConfig,
    pub intervention: InterventionConfig,
    pub workspace: WorkspaceConfig,
    pub allowed_tools: Vec<String>,
    pub blocked_patterns: Vec<String>,
    pub boundaries: BoundariesConfig,
    #[serde(default)]
    pub test_command_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    pub planner: String,
    pub implementer: String,
    pub debugger: String,
    pub security: String,
    pub release: String,
    pub archivist: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    pub max_fix_attempts: u32,
    pub max_tokens_per_task: u64,
    pub max_dollars_per_task: f64,
    pub require_plan_review: bool,
    pub require_pr_review: bool,
    pub max_tool_turns: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterventionConfig {
    pub pause_after_plan: bool,
    pub pause_before_pr: bool,
    pub pause_on_core_change: bool,
    pub pause_on_budget_exceeded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub worktree_dir: String,
    pub task_dir: String,
    pub log_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundariesConfig {
    #[serde(default)]
    pub protected_paths: Vec<String>,
}

impl Default for EngConfig {
    fn default() -> Self {
        Self {
            routing: RoutingConfig {
                planner: "anthropic/claude-haiku-4-5-20251001".into(),
                implementer: "anthropic/claude-sonnet-4-20250514".into(),
                debugger: "anthropic/claude-sonnet-4-20250514".into(),
                security: "anthropic/claude-haiku-4-5-20251001".into(),
                release: "anthropic/claude-haiku-4-5-20251001".into(),
                archivist: "anthropic/claude-haiku-4-5-20251001".into(),
            },
            limits: LimitsConfig {
                max_fix_attempts: 4,
                max_tokens_per_task: 150_000,
                max_dollars_per_task: 10.0,
                require_plan_review: true,
                require_pr_review: true,
                max_tool_turns: 20,
            },
            intervention: InterventionConfig {
                pause_after_plan: true,
                pause_before_pr: true,
                pause_on_core_change: true,
                pause_on_budget_exceeded: true,
            },
            workspace: WorkspaceConfig {
                worktree_dir: ".glitchlab/worktrees".into(),
                task_dir: ".glitchlab/tasks".into(),
                log_dir: ".glitchlab/logs".into(),
            },
            allowed_tools: vec![
                "cargo test".into(),
                "cargo fmt".into(),
                "cargo clippy".into(),
                "cargo build".into(),
                "cargo check".into(),
                "npm test".into(),
                "npm run lint".into(),
                "npm run build".into(),
                "python -m pytest".into(),
                "python -m ruff".into(),
                "go test".into(),
                "go build".into(),
                "go vet".into(),
                "mix test".into(),
                "mix format".into(),
                "git diff".into(),
                "git add".into(),
                "git commit".into(),
                "git status".into(),
                "git log".into(),
                "gh pr create".into(),
                "gh issue view".into(),
                "gh issue list".into(),
                "make".into(),
            ],
            blocked_patterns: vec![
                "rm -rf".into(),
                "curl".into(),
                "wget".into(),
                "sudo".into(),
                "> /dev".into(),
                "| bash".into(),
                "eval(".into(),
                "exec(".into(),
            ],
            boundaries: BoundariesConfig {
                protected_paths: vec![],
            },
            test_command_override: None,
        }
    }
}

impl EngConfig {
    /// Load config with deep merge: built-in defaults + repo overrides.
    pub fn load(repo_path: Option<&Path>) -> Result<Self> {
        let mut config = Self::default();

        if let Some(repo) = repo_path {
            let override_path = repo.join(".glitchlab").join("config.yaml");
            if override_path.exists() {
                let contents = std::fs::read_to_string(&override_path)
                    .map_err(|e| Error::Config(format!("failed to read config: {e}")))?;

                // Parse overrides as a generic YAML value and merge.
                // A YAML file with only comments parses to Null — skip in that case.
                let overrides: serde_yaml::Value = serde_yaml::from_str(&contents)
                    .map_err(|e| Error::Config(format!("invalid config YAML: {e}")))?;

                if !overrides.is_null() {
                    let base: serde_yaml::Value = serde_yaml::to_value(&config)
                        .map_err(|e| Error::Config(format!("failed to serialize defaults: {e}")))?;

                    let merged = deep_merge(base, overrides);
                    config = serde_yaml::from_value(merged).map_err(|e| {
                        Error::Config(format!("failed to parse merged config: {e}"))
                    })?;
                }
            }
        }

        Ok(config)
    }

    /// Build the routing map (role → model string) from config.
    pub fn routing_map(&self) -> HashMap<String, String> {
        HashMap::from([
            ("planner".into(), self.routing.planner.clone()),
            ("implementer".into(), self.routing.implementer.clone()),
            ("debugger".into(), self.routing.debugger.clone()),
            ("security".into(), self.routing.security.clone()),
            ("release".into(), self.routing.release.clone()),
            ("archivist".into(), self.routing.archivist.clone()),
        ])
    }
}

/// Recursively merge override into base (override wins on conflict).
fn deep_merge(base: serde_yaml::Value, over: serde_yaml::Value) -> serde_yaml::Value {
    match (base, over) {
        (serde_yaml::Value::Mapping(mut base_map), serde_yaml::Value::Mapping(over_map)) => {
            for (key, over_val) in over_map {
                let merged = if let Some(base_val) = base_map.remove(&key) {
                    deep_merge(base_val, over_val)
                } else {
                    over_val
                };
                base_map.insert(key, merged);
            }
            serde_yaml::Value::Mapping(base_map)
        }
        (_, over) => over, // Override wins for non-mapping values.
    }
}

/// Check which API keys are available.
pub fn check_api_keys() -> HashMap<String, bool> {
    HashMap::from([
        (
            "ANTHROPIC_API_KEY".into(),
            std::env::var("ANTHROPIC_API_KEY").is_ok(),
        ),
        (
            "GOOGLE_API_KEY".into(),
            std::env::var("GOOGLE_API_KEY").is_ok(),
        ),
        (
            "GEMINI_API_KEY".into(),
            std::env::var("GEMINI_API_KEY").is_ok(),
        ),
        (
            "OPENAI_API_KEY".into(),
            std::env::var("OPENAI_API_KEY").is_ok(),
        ),
    ])
}

/// Auto-detect test command for a repository.
pub fn detect_test_command(repo_path: &Path) -> Option<String> {
    if repo_path.join("Cargo.toml").exists() {
        Some("cargo test".into())
    } else if repo_path.join("package.json").exists() {
        Some("npm test".into())
    } else if repo_path.join("pyproject.toml").exists() || repo_path.join("setup.py").exists() {
        Some("python -m pytest".into())
    } else if repo_path.join("go.mod").exists() {
        Some("go test ./...".into())
    } else if repo_path.join("mix.exs").exists() {
        Some("mix test".into())
    } else if repo_path.join("Makefile").exists() {
        Some("make test".into())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sane_values() {
        let config = EngConfig::default();
        assert_eq!(config.limits.max_fix_attempts, 4);
        assert_eq!(config.limits.max_tokens_per_task, 150_000);
        assert!((config.limits.max_dollars_per_task - 10.0).abs() < f64::EPSILON);
        assert!(config.intervention.pause_after_plan);
        assert!(config.intervention.pause_before_pr);
        assert!(!config.allowed_tools.is_empty());
        assert!(!config.blocked_patterns.is_empty());
        assert!(config.routing.implementer.contains("anthropic"));
        assert!(config.routing.planner.contains("anthropic"));
        assert_eq!(config.limits.max_tool_turns, 20);
    }

    #[test]
    fn routing_map_has_all_roles() {
        let config = EngConfig::default();
        let map = config.routing_map();
        assert!(map.contains_key("planner"));
        assert!(map.contains_key("implementer"));
        assert!(map.contains_key("debugger"));
        assert!(map.contains_key("security"));
        assert!(map.contains_key("release"));
        assert!(map.contains_key("archivist"));
        assert_eq!(map.len(), 6);
    }

    #[test]
    fn load_without_repo_returns_defaults() {
        let config = EngConfig::load(None).unwrap();
        assert_eq!(config.limits.max_fix_attempts, 4);
    }

    #[test]
    fn load_with_nonexistent_override_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngConfig::load(Some(dir.path())).unwrap();
        assert_eq!(config.limits.max_fix_attempts, 4);
    }

    #[test]
    fn load_with_comments_only_config_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let glitchlab_dir = dir.path().join(".glitchlab");
        std::fs::create_dir_all(&glitchlab_dir).unwrap();
        std::fs::write(
            glitchlab_dir.join("config.yaml"),
            "# all commented out\n# routing:\n#   planner: test\n",
        )
        .unwrap();
        let config = EngConfig::load(Some(dir.path())).unwrap();
        assert_eq!(config.limits.max_fix_attempts, 4);
        assert!(config.routing.planner.contains("anthropic"));
    }

    #[test]
    fn load_with_override_merges() {
        let dir = tempfile::tempdir().unwrap();
        let glitchlab_dir = dir.path().join(".glitchlab");
        std::fs::create_dir_all(&glitchlab_dir).unwrap();
        std::fs::write(
            glitchlab_dir.join("config.yaml"),
            "limits:\n  max_fix_attempts: 2\n  max_dollars_per_task: 5.0\n",
        )
        .unwrap();

        let config = EngConfig::load(Some(dir.path())).unwrap();
        assert_eq!(config.limits.max_fix_attempts, 2);
        assert!((config.limits.max_dollars_per_task - 5.0).abs() < f64::EPSILON);
        // Non-overridden values should keep defaults.
        assert_eq!(config.limits.max_tokens_per_task, 150_000);
    }

    #[test]
    fn deep_merge_nested() {
        let base: serde_yaml::Value = serde_yaml::from_str(
            "routing:\n  planner: old\n  implementer: keep\nlimits:\n  max_fix_attempts: 4\n",
        )
        .unwrap();
        let over: serde_yaml::Value =
            serde_yaml::from_str("routing:\n  planner: new\nlimits:\n  max_fix_attempts: 2\n")
                .unwrap();

        let merged = deep_merge(base, over);
        let merged_map = merged.as_mapping().unwrap();
        let routing = merged_map
            .get(serde_yaml::Value::String("routing".into()))
            .unwrap()
            .as_mapping()
            .unwrap();
        assert_eq!(
            routing
                .get(serde_yaml::Value::String("planner".into()))
                .unwrap()
                .as_str()
                .unwrap(),
            "new"
        );
        assert_eq!(
            routing
                .get(serde_yaml::Value::String("implementer".into()))
                .unwrap()
                .as_str()
                .unwrap(),
            "keep"
        );
    }

    #[test]
    fn deep_merge_scalar_override() {
        let base = serde_yaml::Value::String("old".into());
        let over = serde_yaml::Value::String("new".into());
        assert_eq!(
            deep_merge(base, over),
            serde_yaml::Value::String("new".into())
        );
    }

    #[test]
    fn check_api_keys_returns_map() {
        let keys = check_api_keys();
        assert!(keys.contains_key("ANTHROPIC_API_KEY"));
        assert!(keys.contains_key("GOOGLE_API_KEY"));
        assert!(keys.contains_key("GEMINI_API_KEY"));
        assert!(keys.contains_key("OPENAI_API_KEY"));
    }

    #[test]
    fn detect_test_command_rust() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(detect_test_command(dir.path()), Some("cargo test".into()));
    }

    #[test]
    fn detect_test_command_node() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "").unwrap();
        assert_eq!(detect_test_command(dir.path()), Some("npm test".into()));
    }

    #[test]
    fn detect_test_command_python() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(
            detect_test_command(dir.path()),
            Some("python -m pytest".into())
        );
    }

    #[test]
    fn detect_test_command_go() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "").unwrap();
        assert_eq!(
            detect_test_command(dir.path()),
            Some("go test ./...".into())
        );
    }

    #[test]
    fn detect_test_command_elixir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mix.exs"), "").unwrap();
        assert_eq!(detect_test_command(dir.path()), Some("mix test".into()));
    }

    #[test]
    fn detect_test_command_make() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Makefile"), "").unwrap();
        assert_eq!(detect_test_command(dir.path()), Some("make test".into()));
    }

    #[test]
    fn detect_test_command_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect_test_command(dir.path()), None);
    }

    #[test]
    fn config_roundtrip_yaml() {
        let config = EngConfig::default();
        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: EngConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            parsed.limits.max_fix_attempts,
            config.limits.max_fix_attempts
        );
        assert_eq!(parsed.routing.planner, config.routing.planner);
    }
}
