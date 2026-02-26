use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Defines the autonomy level for an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum AutonomyLevel {
    /// Agent requires explicit approval for every action.
    Manual,
    /// Agent can perform actions within predefined boundaries without explicit approval.
    SemiAutonomous,
    /// Agent can operate fully autonomously within its mandate.
    FullyAutonomous,
}

/// Defines an approval gate that must be passed for certain actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalGate {
    /// A descriptive name for the approval gate (e.g., "SecurityReview", "LegalApproval").
    pub name: String,
    /// The minimum number of approvals required to pass this gate.
    pub min_approvals: u32,
    /// Optional list of specific roles or users who can provide approval.
    pub approvers: Option<Vec<String>>,
}

/// Represents the Zephyr policy schema, defining rules for agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ZephyrPolicy {
    /// A map of agent roles to their allowed tools.
    pub tool_allowlist: HashMap<String, Vec<String>>,
    /// A map of agent roles to their assigned autonomy levels.
    pub autonomy_levels: HashMap<String, AutonomyLevel>,
    /// A map of agent roles to the credential scopes they are allowed to access.
    pub credential_scopes: HashMap<String, Vec<String>>,
    /// A map of agent roles to the approval gates they must pass for certain actions.
    pub approval_gates: HashMap<String, Vec<ApprovalGate>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml;

    #[test]
    fn test_zephyr_policy_serialization_deserialization() {
        let mut tool_allowlist = HashMap::new();
        tool_allowlist.insert(
            "Engineer".to_string(),
            vec!["git".to_string(), "cargo".to_string()],
        );

        let mut autonomy_levels = HashMap::new();
        autonomy_levels.insert("Engineer".to_string(), AutonomyLevel::SemiAutonomous);
        autonomy_levels.insert("Reviewer".to_string(), AutonomyLevel::Manual);

        let mut credential_scopes = HashMap::new();
        credential_scopes.insert("Engineer".to_string(), vec!["github_write".to_string()]);

        let mut approval_gates = HashMap::new();
        approval_gates.insert(
            "Engineer".to_string(),
            vec![ApprovalGate {
                name: "CodeReview".to_string(),
                min_approvals: 1,
                approvers: Some(vec!["Reviewer".to_string()]),
            }],
        );

        let policy = ZephyrPolicy {
            tool_allowlist,
            autonomy_levels,
            credential_scopes,
            approval_gates,
        };

        // Test serialization to YAML
        let yaml_string = serde_yaml::to_string(&policy).expect("Failed to serialize to YAML");
        println!("Serialized YAML:\n{}", yaml_string);

        // Test deserialization from YAML
        let deserialized_policy: ZephyrPolicy =
            serde_yaml::from_str(&yaml_string).expect("Failed to deserialize from YAML");

        assert_eq!(policy, deserialized_policy);
    }

    #[test]
    fn test_zephyr_policy_json_schema_generation() {
        let schema = schemars::schema_for!(ZephyrPolicy);
        let schema_json =
            serde_json::to_string_pretty(&schema).expect("Failed to generate JSON schema");
        println!("Generated JSON Schema:\n{}", schema_json);

        // Basic check to ensure the schema contains expected types/fields
        assert!(schema_json.contains("ZephyrPolicy"));
        assert!(schema_json.contains("tool_allowlist"));
        assert!(schema_json.contains("autonomy_levels"));
        assert!(schema_json.contains("credential_scopes"));
        assert!(schema_json.contains("approval_gates"));
        assert!(schema_json.contains("AutonomyLevel"));
        assert!(schema_json.contains("ApprovalGate"));
    }
}
