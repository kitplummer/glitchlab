use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PipelineShape {
    MonitorActReport,
    // Add other pipeline shapes as needed
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ToolAllowlist {
    FlyIoDeploy,
    HealthChecks,
    // Add other allowed tools as needed
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpsGovernanceSettings {
    pub max_retries: u32,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OpsAgentConfig {
    pub pipeline_shape: PipelineShape,
    pub tool_allowlist: Vec<ToolAllowlist>,
    pub governance_settings: OpsGovernanceSettings,
}

impl Default for OpsAgentConfig {
    fn default() -> Self {
        Self {
            pipeline_shape: PipelineShape::MonitorActReport,
            tool_allowlist: vec![ToolAllowlist::FlyIoDeploy, ToolAllowlist::HealthChecks],
            governance_settings: OpsGovernanceSettings {
                max_retries: 3,
                timeout_seconds: 120,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = OpsAgentConfig::default();
        assert_eq!(config.pipeline_shape, PipelineShape::MonitorActReport);
        assert_eq!(
            config.tool_allowlist,
            vec![ToolAllowlist::FlyIoDeploy, ToolAllowlist::HealthChecks]
        );
        assert_eq!(config.governance_settings.max_retries, 3);
        assert_eq!(config.governance_settings.timeout_seconds, 120);
    }

    #[test]
    fn test_config_serialization_deserialization() {
        let config = OpsAgentConfig::default();
        let serialized = serde_json::to_string(&config).unwrap();
        let deserialized: OpsAgentConfig = serde_json::from_str(&serialized).unwrap();
        assert_eq!(config, deserialized);
    }
}
