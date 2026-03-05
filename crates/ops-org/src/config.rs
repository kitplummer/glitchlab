use std::collections::HashMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Pipeline modes (replaces single linear Monitor/Act/Report)
// ---------------------------------------------------------------------------

/// The three fundamental execution modes for operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum OpsPipelineMode {
    Deploy,
    Incident,
    Maintenance,
}

/// Stages within a deploy pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum DeployStage {
    PreCheck,
    Deploy,
    HealthCheck,
    Rollback,
}

/// Stages within an incident pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum IncidentStage {
    Classify,
    Diagnose,
    Respond,
    Postmortem,
}

/// Stages within a maintenance pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum MaintenanceStage {
    Check,
    Report,
}

// ---------------------------------------------------------------------------
// Legacy pipeline stage (backward compat)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum OpsPipelineStage {
    Monitor,
    Act,
    Report,
}

// ---------------------------------------------------------------------------
// Smoke test / deploy target
// ---------------------------------------------------------------------------

/// A declarative endpoint health check definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SmokeTestEndpoint {
    /// HTTP method (e.g. "GET", "POST").
    pub method: String,
    /// URL path (e.g. "/v1/cache/stats").
    pub path: String,
    /// Expected HTTP status code.
    pub expected_status: u16,
    /// Optional substring that must appear in the response body.
    #[serde(default)]
    pub expected_body_contains: Option<String>,
    /// Whether this check must pass for the deploy to be considered healthy.
    pub required: bool,
}

/// A fly.io deploy target configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeployTarget {
    /// Fly.io app name.
    pub app_name: String,
    /// Base URL for health checks (e.g. "https://lowendinsight.fly.dev").
    pub base_url: String,
    /// Smoke test endpoints to verify after deploy.
    pub smoke_tests: Vec<SmokeTestEndpoint>,
    /// Path to fly.toml relative to the repo root (e.g. "apps/lowendinsight_get/fly.toml").
    #[serde(default)]
    pub fly_toml_path: Option<String>,
    /// Fly.io organization slug for `fly apps create -o <org>`.
    #[serde(default)]
    pub fly_org: Option<String>,
}

impl DeployTarget {
    /// Default deploy target for LowEndInsight.
    pub fn lowendinsight_default() -> Self {
        Self {
            app_name: "lowendinsight".into(),
            base_url: "https://lowendinsight.fly.dev".into(),
            fly_toml_path: Some("apps/lowendinsight_get/fly.toml".into()),
            fly_org: None,
            smoke_tests: vec![
                SmokeTestEndpoint {
                    method: "GET".into(),
                    path: "/".into(),
                    expected_status: 200,
                    expected_body_contains: Some("LowEndInsight".into()),
                    required: true,
                },
                SmokeTestEndpoint {
                    method: "GET".into(),
                    path: "/v1/cache/stats".into(),
                    expected_status: 200,
                    expected_body_contains: None,
                    required: true,
                },
                SmokeTestEndpoint {
                    method: "GET".into(),
                    path: "/doc".into(),
                    expected_status: 200,
                    expected_body_contains: None,
                    required: false,
                },
                SmokeTestEndpoint {
                    method: "POST".into(),
                    path: "/v1/analyze".into(),
                    expected_status: 200,
                    expected_body_contains: None,
                    required: false,
                },
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Governance
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OpsGovernanceSettings {
    /// Maximum number of retries for a failed action.
    pub max_action_retries: u8,
    /// Whether to require explicit approval for 'deploy' actions.
    pub require_deploy_approval: bool,
    /// Number of successful deploys completed (for burn-in tracking).
    #[serde(default)]
    pub deploy_count: u32,
    /// Maximum LLM cost in USD per individual ops action.
    #[serde(default = "default_budget_cap")]
    pub budget_cap_per_action: f64,
}

fn default_budget_cap() -> f64 {
    0.50
}

impl Default for OpsGovernanceSettings {
    fn default() -> Self {
        Self {
            max_action_retries: 3,
            require_deploy_approval: true,
            deploy_count: 0,
            budget_cap_per_action: default_budget_cap(),
        }
    }
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

fn default_uplink_sre_model() -> String {
    "anthropic/claude-haiku-4-5-20251001".into()
}

/// Model routing for ops agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OpsRoutingConfig {
    /// Model string for the Uplink SRE assessment agent.
    #[serde(default = "default_uplink_sre_model")]
    pub uplink_sre: String,
}

impl Default for OpsRoutingConfig {
    fn default() -> Self {
        Self {
            uplink_sre: default_uplink_sre_model(),
        }
    }
}

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OpsConfig {
    /// Defines the sequence of stages in the ops agent's pipeline.
    pub pipeline_shape: Vec<OpsPipelineStage>,
    /// A list of tool names that the ops agent is allowed to call.
    pub tool_allowlist: Vec<String>,
    /// Governance settings specific to the ops organization.
    pub governance_settings: OpsGovernanceSettings,
    /// Optional deploy target configuration.
    #[serde(default)]
    pub deploy_target: Option<DeployTarget>,
    /// Model routing for ops agents.
    #[serde(default)]
    pub routing: OpsRoutingConfig,
}

impl Default for OpsConfig {
    fn default() -> Self {
        Self {
            pipeline_shape: vec![
                OpsPipelineStage::Monitor,
                OpsPipelineStage::Act,
                OpsPipelineStage::Report,
            ],
            tool_allowlist: vec!["fly.io deploy".to_string(), "health check".to_string()],
            governance_settings: OpsGovernanceSettings::default(),
            deploy_target: None,
            routing: OpsRoutingConfig::default(),
        }
    }
}

impl OpsConfig {
    /// Load ops config from `<repo>/.glitchlab/ops-config.yaml`.
    ///
    /// Falls back to defaults if the file does not exist.
    pub fn load(repo_path: &Path) -> Result<Self, String> {
        let config_path = repo_path.join(".glitchlab/ops-config.yaml");
        if !config_path.exists() {
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("failed to read ops config: {e}"))?;
        serde_yaml::from_str(&contents).map_err(|e| format!("failed to parse ops config: {e}"))
    }

    /// Build the routing map (role → model string) for the router.
    pub fn routing_map(&self) -> HashMap<String, String> {
        HashMap::from([("uplink_sre".into(), self.routing.uplink_sre.clone())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_ops_governance_settings() {
        let settings = OpsGovernanceSettings::default();
        assert_eq!(settings.max_action_retries, 3);
        assert!(settings.require_deploy_approval);
        assert_eq!(settings.deploy_count, 0);
        assert!((settings.budget_cap_per_action - 0.50).abs() < f64::EPSILON);
    }

    #[test]
    fn test_default_ops_config() {
        let config = OpsConfig::default();
        assert_eq!(
            config.pipeline_shape,
            vec![
                OpsPipelineStage::Monitor,
                OpsPipelineStage::Act,
                OpsPipelineStage::Report
            ]
        );
        assert_eq!(
            config.tool_allowlist,
            vec!["fly.io deploy".to_string(), "health check".to_string()]
        );
        assert_eq!(config.governance_settings, OpsGovernanceSettings::default());
        assert!(config.deploy_target.is_none());
    }

    #[test]
    fn test_config_serialization_deserialization() {
        let config = OpsConfig::default();
        let serialized = serde_json::to_string(&config).unwrap();
        let deserialized: OpsConfig = serde_json::from_str(&serialized).unwrap();
        assert_eq!(config, deserialized);

        let governance_settings = OpsGovernanceSettings::default();
        let serialized_gov = serde_json::to_string(&governance_settings).unwrap();
        let deserialized_gov: OpsGovernanceSettings =
            serde_json::from_str(&serialized_gov).unwrap();
        assert_eq!(governance_settings, deserialized_gov);
    }

    #[test]
    fn test_pipeline_modes_serde() {
        let modes = vec![
            OpsPipelineMode::Deploy,
            OpsPipelineMode::Incident,
            OpsPipelineMode::Maintenance,
        ];
        let json = serde_json::to_string(&modes).unwrap();
        let parsed: Vec<OpsPipelineMode> = serde_json::from_str(&json).unwrap();
        assert_eq!(modes, parsed);
    }

    #[test]
    fn test_deploy_stages_serde() {
        let stages = vec![
            DeployStage::PreCheck,
            DeployStage::Deploy,
            DeployStage::HealthCheck,
            DeployStage::Rollback,
        ];
        let json = serde_json::to_string(&stages).unwrap();
        let parsed: Vec<DeployStage> = serde_json::from_str(&json).unwrap();
        assert_eq!(stages, parsed);
    }

    #[test]
    fn test_incident_stages_serde() {
        let stages = vec![
            IncidentStage::Classify,
            IncidentStage::Diagnose,
            IncidentStage::Respond,
            IncidentStage::Postmortem,
        ];
        let json = serde_json::to_string(&stages).unwrap();
        let parsed: Vec<IncidentStage> = serde_json::from_str(&json).unwrap();
        assert_eq!(stages, parsed);
    }

    #[test]
    fn test_maintenance_stages_serde() {
        let stages = vec![MaintenanceStage::Check, MaintenanceStage::Report];
        let json = serde_json::to_string(&stages).unwrap();
        let parsed: Vec<MaintenanceStage> = serde_json::from_str(&json).unwrap();
        assert_eq!(stages, parsed);
    }

    #[test]
    fn test_lowendinsight_default_deploy_target() {
        let target = DeployTarget::lowendinsight_default();
        assert_eq!(target.app_name, "lowendinsight");
        assert_eq!(target.base_url, "https://lowendinsight.fly.dev");
        assert_eq!(target.smoke_tests.len(), 4);
        assert_eq!(
            target.fly_toml_path.as_deref(),
            Some("apps/lowendinsight_get/fly.toml")
        );
        assert!(target.fly_org.is_none());

        // Two required checks
        let required: Vec<_> = target.smoke_tests.iter().filter(|s| s.required).collect();
        assert_eq!(required.len(), 2);

        // Two optional checks
        let optional: Vec<_> = target.smoke_tests.iter().filter(|s| !s.required).collect();
        assert_eq!(optional.len(), 2);
    }

    #[test]
    fn test_deploy_target_fly_toml_path_serde_default() {
        // Deserialize without fly_toml_path or fly_org — should default to None
        let json = r#"{"app_name":"test","base_url":"https://test.dev","smoke_tests":[]}"#;
        let target: DeployTarget = serde_json::from_str(json).unwrap();
        assert!(target.fly_toml_path.is_none());
        assert!(target.fly_org.is_none());
    }

    #[test]
    fn test_deploy_target_fly_org_serde_roundtrip() {
        let target = DeployTarget {
            app_name: "myapp".into(),
            base_url: "https://myapp.fly.dev".into(),
            smoke_tests: vec![],
            fly_toml_path: Some("fly.toml".into()),
            fly_org: Some("my-org".into()),
        };
        let json = serde_json::to_string(&target).unwrap();
        let parsed: DeployTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.fly_toml_path.as_deref(), Some("fly.toml"));
        assert_eq!(parsed.fly_org.as_deref(), Some("my-org"));
    }

    #[test]
    fn test_deploy_target_serde_roundtrip() {
        let target = DeployTarget::lowendinsight_default();
        let json = serde_json::to_string(&target).unwrap();
        let parsed: DeployTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(target, parsed);
    }

    #[test]
    fn test_smoke_test_endpoint_serde() {
        let endpoint = SmokeTestEndpoint {
            method: "GET".into(),
            path: "/health".into(),
            expected_status: 200,
            expected_body_contains: Some("ok".into()),
            required: true,
        };
        let json = serde_json::to_string(&endpoint).unwrap();
        let parsed: SmokeTestEndpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(endpoint, parsed);
    }

    #[test]
    fn test_config_with_deploy_target() {
        let config = OpsConfig {
            deploy_target: Some(DeployTarget::lowendinsight_default()),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: OpsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
        assert!(parsed.deploy_target.is_some());
    }

    #[test]
    fn test_ops_routing_config_default() {
        let routing = OpsRoutingConfig::default();
        assert!(routing.uplink_sre.contains("haiku"));
    }

    #[test]
    fn test_ops_routing_config_serde() {
        let routing = OpsRoutingConfig::default();
        let json = serde_json::to_string(&routing).unwrap();
        let parsed: OpsRoutingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(routing, parsed);
    }

    #[test]
    fn test_ops_config_routing_map() {
        let config = OpsConfig::default();
        let map = config.routing_map();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("uplink_sre"));
    }

    #[test]
    fn test_ops_config_load_missing_file() {
        let config = OpsConfig::load(std::path::Path::new("/nonexistent/path")).unwrap();
        assert_eq!(config, OpsConfig::default());
    }

    #[test]
    fn test_ops_config_load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let gl_dir = dir.path().join(".glitchlab");
        std::fs::create_dir_all(&gl_dir).unwrap();
        let yaml = r#"
pipeline_shape: [Monitor, Act, Report]
tool_allowlist: ["fly.io deploy"]
governance_settings:
  max_action_retries: 5
  require_deploy_approval: false
  budget_cap_per_action: 1.0
routing:
  uplink_sre: "anthropic/claude-sonnet-4-20250514"
"#;
        std::fs::write(gl_dir.join("ops-config.yaml"), yaml).unwrap();
        let config = OpsConfig::load(dir.path()).unwrap();
        assert_eq!(config.governance_settings.max_action_retries, 5);
        assert!(!config.governance_settings.require_deploy_approval);
        assert_eq!(
            config.routing.uplink_sre,
            "anthropic/claude-sonnet-4-20250514"
        );
    }

    #[test]
    fn test_ops_config_load_invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let gl_dir = dir.path().join(".glitchlab");
        std::fs::create_dir_all(&gl_dir).unwrap();
        std::fs::write(gl_dir.join("ops-config.yaml"), "{{invalid").unwrap();
        let result = OpsConfig::load(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_default_config_includes_routing() {
        let config = OpsConfig::default();
        assert_eq!(config.routing, OpsRoutingConfig::default());
    }
}
