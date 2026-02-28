use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Represents a specific expectation for an agent's behavior or output.
///
/// This is used in evaluations to define what is being measured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Expectation {
    /// The agent should produce a specific output.
    ProducesOutput(String),
    /// The agent should not produce a specific output.
    DoesNotProduceOutput(String),
    /// The agent should call a specific tool.
    CallsTool(String),
    /// The agent should not call a specific tool.
    DoesNotCallTool(String),
    /// The agent should complete within a certain budget.
    CompletesWithinBudget,
    /// The agent should handle an error gracefully.
    HandlesError,
}

/// Defines a scenario for evaluating an agent.
///
/// It includes the context given to the agent and the expectations for its
/// performance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextScenario {
    /// A unique identifier for the scenario.
    pub id: String,
    /// The context or prompt provided to the agent.
    pub context: String,
    /// A list of expectations to evaluate against.
    pub expectations: Vec<Expectation>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scenario_serialization() {
        let scenario = ContextScenario {
            id: "test-1".to_string(),
            context: "This is a test context.".to_string(),
            expectations: vec![
                Expectation::ProducesOutput("Hello, world!".to_string()),
                Expectation::CallsTool("read_file".to_string()),
            ],
        };

        let serialized = serde_json::to_string(&scenario).unwrap();
        let deserialized: ContextScenario = serde_json::from_str(&serialized).unwrap();

        assert_eq!(scenario, deserialized);
    }
}
