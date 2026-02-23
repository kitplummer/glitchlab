use std::collections::HashMap;
use std::path::Path;

use glitchlab_kernel::error::{Error, Result};
use glitchlab_router::ProviderInit;
use glitchlab_router::chooser::{ModelChooser, ModelProfile, ModelTier, RolePreference};
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
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfigEntry>,
}

/// Configuration for the memory/persistence layer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// MySQL/Dolt connection string. If `None`, Dolt is not used.
    #[serde(default)]
    pub dolt_connection: Option<String>,
    /// Whether to enable the Beads graph backend.
    #[serde(default)]
    pub beads_enabled: bool,
    /// Path to the `bd` binary. If `None`, uses `BEADS_BD_PATH` or `"bd"` on PATH.
    #[serde(default)]
    pub beads_bd_path: Option<String>,
}

impl MemoryConfig {
    /// Convert to the memory crate's config type.
    pub fn to_backend_config(&self) -> glitchlab_memory::MemoryBackendConfig {
        glitchlab_memory::MemoryBackendConfig {
            dolt_connection: self.dolt_connection.clone(),
            beads_enabled: self.beads_enabled,
            beads_bd_path: self.beads_bd_path.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    pub planner: String,
    pub implementer: String,
    pub debugger: String,
    pub security: String,
    pub release: String,
    pub archivist: String,

    /// Model pool for cost-aware chooser. When non-empty, enables
    /// the `ModelChooser` for dynamic model selection.
    #[serde(default)]
    pub models: Vec<ModelProfileConfig>,

    /// Per-role preferences for cost-aware routing.
    #[serde(default)]
    pub roles: HashMap<String, RolePreferenceConfig>,

    /// Budget pressure sensitivity. 0.0 = quality-first, 1.0 = cost-first.
    #[serde(default = "default_cost_quality_threshold")]
    pub cost_quality_threshold: f64,
}

fn default_cost_quality_threshold() -> f64 {
    0.5
}

/// Configuration for a model in the cost-aware pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProfileConfig {
    pub model: String,
    pub tier: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub input_cost_per_m: Option<f64>,
    #[serde(default)]
    pub output_cost_per_m: Option<f64>,
}

/// Configuration for per-role model preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolePreferenceConfig {
    #[serde(default = "default_min_tier")]
    pub min_tier: String,
    #[serde(default)]
    pub requires: Vec<String>,
}

fn default_min_tier() -> String {
    "economy".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    pub max_fix_attempts: u32,
    pub max_tokens_per_task: u64,
    pub max_dollars_per_task: f64,
    pub require_plan_review: bool,
    pub require_pr_review: bool,
    pub max_tool_turns: u32,
    pub max_pipeline_duration_secs: u64,
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

/// Configuration for a single LLM provider (from config or secrets file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfigEntry {
    /// Provider kind (e.g. "anthropic", "gemini", "openai"). Defaults to the map key.
    #[serde(default)]
    pub kind: Option<String>,
    /// Inline API key (typically from secrets.yaml).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Custom env var name to read the API key from.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Base URL override.
    #[serde(default)]
    pub base_url: Option<String>,
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
                models: Vec::new(),
                roles: HashMap::new(),
                cost_quality_threshold: default_cost_quality_threshold(),
            },
            limits: LimitsConfig {
                max_fix_attempts: 2,
                max_tokens_per_task: 150_000,
                max_dollars_per_task: 2.0,
                require_plan_review: true,
                require_pr_review: true,
                max_tool_turns: 20,
                max_pipeline_duration_secs: 600,
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
            memory: MemoryConfig::default(),
            providers: HashMap::new(),
        }
    }
}

impl EngConfig {
    /// Load config with deep merge: built-in defaults + repo overrides + secrets.
    pub fn load(repo_path: Option<&Path>) -> Result<Self> {
        let mut config = Self::default();

        if let Some(repo) = repo_path {
            let glitchlab_dir = repo.join(".glitchlab");
            let override_path = glitchlab_dir.join("config.yaml");
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

            // Deep-merge secrets.yaml (if present) on top of config.
            let secrets_path = glitchlab_dir.join("secrets.yaml");
            if secrets_path.exists() {
                let contents = std::fs::read_to_string(&secrets_path)
                    .map_err(|e| Error::Config(format!("failed to read secrets: {e}")))?;

                let secrets: serde_yaml::Value = serde_yaml::from_str(&contents)
                    .map_err(|e| Error::Config(format!("invalid secrets YAML: {e}")))?;

                if !secrets.is_null() {
                    let base: serde_yaml::Value = serde_yaml::to_value(&config)
                        .map_err(|e| Error::Config(format!("failed to serialize config: {e}")))?;

                    let merged = deep_merge(base, secrets);
                    config = serde_yaml::from_value(merged).map_err(|e| {
                        Error::Config(format!("failed to parse merged secrets: {e}"))
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

    /// Build a `ModelChooser` from the config's model pool and role preferences.
    /// Returns `None` if no models are configured (use static routing instead).
    pub fn build_chooser(&self) -> Option<ModelChooser> {
        if self.routing.models.is_empty() {
            return None;
        }

        let models: Vec<ModelProfile> = self
            .routing
            .models
            .iter()
            .filter_map(|cfg| {
                let tier = ModelTier::from_str_loose(&cfg.tier)?;
                let (default_input, default_output) = default_cost(&cfg.model);
                Some(ModelProfile {
                    model_string: cfg.model.clone(),
                    input_cost_per_m: cfg.input_cost_per_m.unwrap_or(default_input),
                    output_cost_per_m: cfg.output_cost_per_m.unwrap_or(default_output),
                    tier,
                    capabilities: cfg.capabilities.iter().cloned().collect(),
                })
            })
            .collect();

        let role_preferences: HashMap<String, RolePreference> = self
            .routing
            .roles
            .iter()
            .filter_map(|(role, cfg)| {
                let tier = ModelTier::from_str_loose(&cfg.min_tier)?;
                Some((
                    role.clone(),
                    RolePreference {
                        min_tier: tier,
                        required_capabilities: cfg.requires.iter().cloned().collect(),
                    },
                ))
            })
            .collect();

        Some(ModelChooser::new(
            models,
            role_preferences,
            self.routing.cost_quality_threshold,
        ))
    }

    /// Resolve provider configs into `ProviderInit` values for the router.
    ///
    /// Resolution order per entry:
    /// 1. `api_key` field (inline literal, typically from secrets.yaml)
    /// 2. env var named by `api_key_env`
    /// 3. default env var(s) for the provider kind
    ///
    /// Only entries with a resolvable key are included.
    pub fn resolve_providers(&self) -> HashMap<String, ProviderInit> {
        let mut result = HashMap::new();

        for (name, entry) in &self.providers {
            let kind = entry.kind.clone().unwrap_or_else(|| name.clone());

            let api_key = entry
                .api_key
                .clone()
                .or_else(|| {
                    entry
                        .api_key_env
                        .as_ref()
                        .and_then(|var| std::env::var(var).ok())
                })
                .or_else(|| resolve_default_env_key(&kind));

            if let Some(api_key) = api_key {
                result.insert(
                    name.clone(),
                    ProviderInit {
                        kind,
                        api_key,
                        base_url: entry.base_url.clone(),
                        name: Some(name.clone()),
                    },
                );
            }
        }

        result
    }
}

/// Look up the default env var(s) for a known provider kind.
fn resolve_default_env_key(kind: &str) -> Option<String> {
    match kind {
        "anthropic" => std::env::var("ANTHROPIC_API_KEY").ok(),
        "gemini" => std::env::var("GOOGLE_API_KEY")
            .or_else(|_| std::env::var("GEMINI_API_KEY"))
            .ok(),
        "openai" => std::env::var("OPENAI_API_KEY").ok(),
        _ => None,
    }
}

/// Infer default cost per million tokens from model string.
/// Used when costs are not explicitly set in config.
fn default_cost(model_string: &str) -> (f64, f64) {
    let lower = model_string.to_lowercase();
    if lower.contains("flash-lite") {
        (0.075, 0.30)
    } else if lower.contains("flash") {
        (0.15, 0.60)
    } else if lower.contains("gemini-2.5-pro") {
        (1.25, 10.0)
    } else if lower.contains("haiku") {
        (0.80, 4.0)
    } else if lower.contains("sonnet") {
        (3.0, 15.0)
    } else if lower.contains("opus") {
        (15.0, 75.0)
    } else if lower.contains("gpt-4o-mini") {
        (0.15, 0.60)
    } else if lower.contains("gpt-4o") {
        (2.50, 10.0)
    } else {
        // Unknown model — assume mid-range.
        (1.0, 5.0)
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
        assert_eq!(config.limits.max_fix_attempts, 2);
        assert_eq!(config.limits.max_tokens_per_task, 150_000);
        assert!((config.limits.max_dollars_per_task - 2.0).abs() < f64::EPSILON);
        assert!(config.intervention.pause_after_plan);
        assert!(config.intervention.pause_before_pr);
        assert!(!config.allowed_tools.is_empty());
        assert!(!config.blocked_patterns.is_empty());
        assert!(config.routing.implementer.contains("anthropic"));
        assert!(config.routing.planner.contains("anthropic"));
        assert_eq!(config.limits.max_tool_turns, 20);
        assert_eq!(config.limits.max_pipeline_duration_secs, 600);
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
        assert_eq!(config.limits.max_fix_attempts, 2);
    }

    #[test]
    fn load_with_nonexistent_override_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngConfig::load(Some(dir.path())).unwrap();
        assert_eq!(config.limits.max_fix_attempts, 2);
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
        assert_eq!(config.limits.max_fix_attempts, 2);
        assert!(config.routing.planner.contains("anthropic"));
    }

    #[test]
    fn load_with_override_merges() {
        let dir = tempfile::tempdir().unwrap();
        let glitchlab_dir = dir.path().join(".glitchlab");
        std::fs::create_dir_all(&glitchlab_dir).unwrap();
        std::fs::write(
            glitchlab_dir.join("config.yaml"),
            "limits:\n  max_fix_attempts: 5\n  max_dollars_per_task: 5.0\n",
        )
        .unwrap();

        let config = EngConfig::load(Some(dir.path())).unwrap();
        assert_eq!(config.limits.max_fix_attempts, 5);
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

    #[test]
    fn memory_config_default_has_all_disabled() {
        let config = MemoryConfig::default();
        assert!(config.dolt_connection.is_none());
        assert!(!config.beads_enabled);
        assert!(config.beads_bd_path.is_none());
    }

    #[test]
    fn memory_config_yaml_roundtrip() {
        let config = MemoryConfig {
            dolt_connection: Some("mysql://localhost:3306/glitchlab".into()),
            beads_enabled: true,
            beads_bd_path: Some("/usr/local/bin/bd".into()),
        };
        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: MemoryConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.dolt_connection, config.dolt_connection);
        assert_eq!(parsed.beads_enabled, config.beads_enabled);
        assert_eq!(parsed.beads_bd_path, config.beads_bd_path);
    }

    #[test]
    fn memory_config_to_backend_config() {
        let config = MemoryConfig {
            dolt_connection: Some("mysql://test".into()),
            beads_enabled: true,
            beads_bd_path: None,
        };
        let bc = config.to_backend_config();
        assert_eq!(bc.dolt_connection, Some("mysql://test".into()));
        assert!(bc.beads_enabled);
        assert!(bc.beads_bd_path.is_none());
    }

    #[test]
    fn eng_config_with_memory_section_parses() {
        let yaml = r#"
routing:
  planner: "anthropic/claude-haiku-4-5-20251001"
  implementer: "anthropic/claude-sonnet-4-20250514"
  debugger: "anthropic/claude-sonnet-4-20250514"
  security: "anthropic/claude-haiku-4-5-20251001"
  release: "anthropic/claude-haiku-4-5-20251001"
  archivist: "anthropic/claude-haiku-4-5-20251001"
limits:
  max_fix_attempts: 4
  max_tokens_per_task: 150000
  max_dollars_per_task: 10.0
  require_plan_review: true
  require_pr_review: true
  max_tool_turns: 20
  max_pipeline_duration_secs: 3600
intervention:
  pause_after_plan: true
  pause_before_pr: true
  pause_on_core_change: true
  pause_on_budget_exceeded: true
workspace:
  worktree_dir: ".glitchlab/worktrees"
  task_dir: ".glitchlab/tasks"
  log_dir: ".glitchlab/logs"
allowed_tools: []
blocked_patterns: []
boundaries:
  protected_paths: []
memory:
  dolt_connection: "mysql://localhost:3306/glitchlab"
  beads_enabled: true
  beads_bd_path: "/opt/bd"
"#;
        let config: EngConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            config.memory.dolt_connection,
            Some("mysql://localhost:3306/glitchlab".into())
        );
        assert!(config.memory.beads_enabled);
        assert_eq!(config.memory.beads_bd_path, Some("/opt/bd".into()));
    }

    // -----------------------------------------------------------------------
    // ModelChooser config tests
    // -----------------------------------------------------------------------

    #[test]
    fn routing_config_with_models_parses() {
        let yaml = r#"
routing:
  planner: "gemini/gemini-2.5-flash"
  implementer: "gemini/gemini-2.5-flash"
  debugger: "gemini/gemini-2.5-flash"
  security: "gemini/gemini-2.5-flash"
  release: "gemini/gemini-2.5-flash"
  archivist: "gemini/gemini-2.5-flash"
  models:
    - model: "gemini/gemini-2.5-flash-lite"
      tier: economy
      capabilities: [tool_use, code]
    - model: "gemini/gemini-2.5-flash"
      tier: standard
      capabilities: [tool_use, code, long_context]
  roles:
    planner:
      min_tier: standard
    debugger:
      min_tier: economy
      requires: [tool_use]
  cost_quality_threshold: 0.7
limits:
  max_fix_attempts: 2
  max_tokens_per_task: 150000
  max_dollars_per_task: 2.0
  require_plan_review: true
  require_pr_review: true
  max_tool_turns: 20
  max_pipeline_duration_secs: 600
intervention:
  pause_after_plan: true
  pause_before_pr: true
  pause_on_core_change: true
  pause_on_budget_exceeded: true
workspace:
  worktree_dir: ".glitchlab/worktrees"
  task_dir: ".glitchlab/tasks"
  log_dir: ".glitchlab/logs"
allowed_tools: []
blocked_patterns: []
boundaries:
  protected_paths: []
"#;
        let config: EngConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.routing.models.len(), 2);
        assert_eq!(config.routing.models[0].tier, "economy");
        assert_eq!(config.routing.roles.len(), 2);
        assert!((config.routing.cost_quality_threshold - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn routing_config_without_models_backward_compat() {
        let config = EngConfig::default();
        assert!(config.routing.models.is_empty());
        assert!(config.routing.roles.is_empty());
        assert!((config.routing.cost_quality_threshold - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn build_chooser_returns_none_without_models() {
        let config = EngConfig::default();
        assert!(config.build_chooser().is_none());
    }

    #[test]
    fn build_chooser_returns_some_with_models() {
        let mut config = EngConfig::default();
        config.routing.models = vec![
            ModelProfileConfig {
                model: "gemini/gemini-2.5-flash-lite".into(),
                tier: "economy".into(),
                capabilities: vec!["tool_use".into()],
                input_cost_per_m: None,
                output_cost_per_m: None,
            },
            ModelProfileConfig {
                model: "gemini/gemini-2.5-flash".into(),
                tier: "standard".into(),
                capabilities: vec!["tool_use".into(), "code".into()],
                input_cost_per_m: Some(0.15),
                output_cost_per_m: Some(0.60),
            },
        ];
        config.routing.roles.insert(
            "planner".into(),
            RolePreferenceConfig {
                min_tier: "standard".into(),
                requires: vec![],
            },
        );

        let chooser = config.build_chooser();
        assert!(chooser.is_some());
        let chooser = chooser.unwrap();
        assert!(!chooser.is_empty());
    }

    #[test]
    fn cost_quality_threshold_defaults() {
        let yaml = r#"
routing:
  planner: "x"
  implementer: "x"
  debugger: "x"
  security: "x"
  release: "x"
  archivist: "x"
limits:
  max_fix_attempts: 2
  max_tokens_per_task: 150000
  max_dollars_per_task: 2.0
  require_plan_review: true
  require_pr_review: true
  max_tool_turns: 20
  max_pipeline_duration_secs: 600
intervention:
  pause_after_plan: true
  pause_before_pr: true
  pause_on_core_change: true
  pause_on_budget_exceeded: true
workspace:
  worktree_dir: "."
  task_dir: "."
  log_dir: "."
allowed_tools: []
blocked_patterns: []
boundaries:
  protected_paths: []
"#;
        let config: EngConfig = serde_yaml::from_str(yaml).unwrap();
        assert!((config.routing.cost_quality_threshold - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn build_chooser_skips_invalid_tier() {
        let mut config = EngConfig::default();
        config.routing.models = vec![
            ModelProfileConfig {
                model: "gemini/gemini-2.5-flash-lite".into(),
                tier: "invalid_tier".into(),
                capabilities: vec![],
                input_cost_per_m: None,
                output_cost_per_m: None,
            },
            ModelProfileConfig {
                model: "gemini/gemini-2.5-flash".into(),
                tier: "standard".into(),
                capabilities: vec![],
                input_cost_per_m: None,
                output_cost_per_m: None,
            },
        ];

        let chooser = config.build_chooser().unwrap();
        // Only one model should make it through (the one with valid tier).
        let selected = chooser.select("any", 10.0, 10.0);
        assert_eq!(selected, Some("gemini/gemini-2.5-flash"));
    }

    #[test]
    fn default_cost_inference() {
        let (i, o) = default_cost("gemini/gemini-2.5-flash-lite");
        assert!((i - 0.075).abs() < f64::EPSILON);
        assert!((o - 0.30).abs() < f64::EPSILON);

        let (i, o) = default_cost("gemini/gemini-2.5-flash");
        assert!((i - 0.15).abs() < f64::EPSILON);
        assert!((o - 0.60).abs() < f64::EPSILON);

        let (i, o) = default_cost("anthropic/claude-sonnet-4-20250514");
        assert!((i - 3.0).abs() < f64::EPSILON);
        assert!((o - 15.0).abs() < f64::EPSILON);

        let (i, o) = default_cost("unknown/model");
        assert!((i - 1.0).abs() < f64::EPSILON);
        assert!((o - 5.0).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Provider config tests
    // -----------------------------------------------------------------------

    #[test]
    fn provider_config_entry_deserializes() {
        let yaml = r#"
kind: anthropic
api_key: sk-test-123
api_key_env: MY_CUSTOM_KEY
base_url: http://localhost:8080
"#;
        let entry: ProviderConfigEntry = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(entry.kind.as_deref(), Some("anthropic"));
        assert_eq!(entry.api_key.as_deref(), Some("sk-test-123"));
        assert_eq!(entry.api_key_env.as_deref(), Some("MY_CUSTOM_KEY"));
        assert_eq!(entry.base_url.as_deref(), Some("http://localhost:8080"));

        // Roundtrip.
        let serialized = serde_yaml::to_string(&entry).unwrap();
        let roundtrip: ProviderConfigEntry = serde_yaml::from_str(&serialized).unwrap();
        assert_eq!(roundtrip.kind, entry.kind);
        assert_eq!(roundtrip.api_key, entry.api_key);
    }

    #[test]
    fn provider_config_empty_defaults() {
        let yaml = "providers: {}\n";
        // Parse a minimal config with empty providers.
        let config = EngConfig::default();
        let base: serde_yaml::Value = serde_yaml::to_value(&config).unwrap();
        let over: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let merged = deep_merge(base, over);
        let parsed: EngConfig = serde_yaml::from_value(merged).unwrap();
        assert!(parsed.providers.is_empty());
    }

    #[test]
    fn resolve_providers_inline_key() {
        let mut config = EngConfig::default();
        config.providers.insert(
            "anthropic".into(),
            ProviderConfigEntry {
                kind: None,
                api_key: Some("sk-inline".into()),
                api_key_env: None,
                base_url: None,
            },
        );

        let resolved = config.resolve_providers();
        assert!(resolved.contains_key("anthropic"));
        assert_eq!(resolved["anthropic"].api_key, "sk-inline");
        assert_eq!(resolved["anthropic"].kind, "anthropic");
    }

    #[test]
    fn resolve_providers_custom_env_var() {
        unsafe {
            std::env::set_var("GLITCHLAB_TEST_KEY_CUSTOM", "from-custom-env");
        }

        let mut config = EngConfig::default();
        config.providers.insert(
            "anthropic".into(),
            ProviderConfigEntry {
                kind: None,
                api_key: None,
                api_key_env: Some("GLITCHLAB_TEST_KEY_CUSTOM".into()),
                base_url: None,
            },
        );

        let resolved = config.resolve_providers();
        assert!(resolved.contains_key("anthropic"));
        assert_eq!(resolved["anthropic"].api_key, "from-custom-env");

        unsafe {
            std::env::remove_var("GLITCHLAB_TEST_KEY_CUSTOM");
        }
    }

    #[test]
    fn resolve_providers_default_env_fallback() {
        // Set the default env var for anthropic.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "default-env-key");
        }

        let mut config = EngConfig::default();
        config.providers.insert(
            "anthropic".into(),
            ProviderConfigEntry {
                kind: None,
                api_key: None,
                api_key_env: None,
                base_url: None,
            },
        );

        let resolved = config.resolve_providers();
        assert!(resolved.contains_key("anthropic"));
        assert_eq!(resolved["anthropic"].api_key, "default-env-key");
    }

    #[test]
    fn resolve_providers_priority_order() {
        // Inline key should take precedence over env var.
        unsafe {
            std::env::set_var("GLITCHLAB_TEST_KEY_PRIO", "from-env");
        }

        let mut config = EngConfig::default();
        config.providers.insert(
            "anthropic".into(),
            ProviderConfigEntry {
                kind: None,
                api_key: Some("inline-wins".into()),
                api_key_env: Some("GLITCHLAB_TEST_KEY_PRIO".into()),
                base_url: None,
            },
        );

        let resolved = config.resolve_providers();
        assert_eq!(resolved["anthropic"].api_key, "inline-wins");

        unsafe {
            std::env::remove_var("GLITCHLAB_TEST_KEY_PRIO");
        }
    }

    #[test]
    fn resolve_providers_skips_unresolvable() {
        let mut config = EngConfig::default();
        config.providers.insert(
            "custom-local".into(),
            ProviderConfigEntry {
                kind: Some("ollama".into()),
                api_key: None,
                api_key_env: Some("NONEXISTENT_GLITCHLAB_VAR_12345".into()),
                base_url: Some("http://localhost:11434/v1".into()),
            },
        );

        let resolved = config.resolve_providers();
        // No key resolvable → should be omitted.
        assert!(!resolved.contains_key("custom-local"));
    }

    #[test]
    fn resolve_providers_custom_kind() {
        let mut config = EngConfig::default();
        config.providers.insert(
            "my-ollama".into(),
            ProviderConfigEntry {
                kind: Some("openai".into()),
                api_key: Some("dummy".into()),
                api_key_env: None,
                base_url: Some("http://localhost:11434/v1".into()),
            },
        );

        let resolved = config.resolve_providers();
        assert!(resolved.contains_key("my-ollama"));
        assert_eq!(resolved["my-ollama"].kind, "openai");
        assert_eq!(resolved["my-ollama"].name.as_deref(), Some("my-ollama"));
    }

    #[test]
    fn secrets_file_merges_into_config() {
        let dir = tempfile::tempdir().unwrap();
        let glitchlab_dir = dir.path().join(".glitchlab");
        std::fs::create_dir_all(&glitchlab_dir).unwrap();

        // Main config with provider entry (no api_key).
        std::fs::write(
            glitchlab_dir.join("config.yaml"),
            "providers:\n  anthropic:\n    api_key_env: ANTHROPIC_API_KEY\n",
        )
        .unwrap();

        // Secrets file with inline key.
        std::fs::write(
            glitchlab_dir.join("secrets.yaml"),
            "providers:\n  anthropic:\n    api_key: sk-secret-123\n",
        )
        .unwrap();

        let config = EngConfig::load(Some(dir.path())).unwrap();
        assert_eq!(
            config.providers["anthropic"].api_key.as_deref(),
            Some("sk-secret-123")
        );
        // The api_key_env from config should also still be there.
        assert_eq!(
            config.providers["anthropic"].api_key_env.as_deref(),
            Some("ANTHROPIC_API_KEY")
        );
    }

    #[test]
    fn secrets_file_missing_no_error() {
        let dir = tempfile::tempdir().unwrap();
        let glitchlab_dir = dir.path().join(".glitchlab");
        std::fs::create_dir_all(&glitchlab_dir).unwrap();
        std::fs::write(
            glitchlab_dir.join("config.yaml"),
            "limits:\n  max_fix_attempts: 3\n",
        )
        .unwrap();
        // No secrets.yaml — should be fine.
        let config = EngConfig::load(Some(dir.path())).unwrap();
        assert_eq!(config.limits.max_fix_attempts, 3);
    }

    #[test]
    fn backward_compat_no_providers_section() {
        // Existing configs without `providers:` should still parse.
        let config = EngConfig::default();
        assert!(config.providers.is_empty());

        // From YAML without providers section.
        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: EngConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(parsed.providers.is_empty());
    }
}
