use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum OpsPipelineStage {
    Monitor,
    Act,
    Report,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OpsGovernanceSettings {
    /// Maximum number of retries for a failed action.
    pub max_action_retries: u8,
    /// Whether to require explicit approval for 'deploy' actions.
    pub require_deploy_approval: bool,
}

impl Default for OpsGovernanceSettings {
    fn default() -> Self {
        Self {
            max_action_retries: 3,
            require_deploy_approval: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OpsConfig {
    /// Defines the sequence of stages in the ops agent's pipeline.
    pub pipeline_shape: Vec<OpsPipelineStage>,
    /// A list of tool names that the ops agent is allowed to call.
    pub tool_allowlist: Vec<String>,
    /// Governance settings specific to the ops organization.
    pub governance_settings: OpsGovernanceSettings,
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
        }
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
}
