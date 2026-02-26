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
pub struct OpsOrgConfig {
    pub pipeline_shape: PipelineShape,
    pub tool_allowlist: Vec<ToolAllowlist>,
    pub governance_settings: OpsGovernanceSettings,
}
