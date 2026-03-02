use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Token usage metrics captured during a scenario evaluation run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokenUsage {
    /// Tokens in the input/prompt.
    pub input_tokens: u64,
    /// Tokens in the completion/output.
    pub output_tokens: u64,
    /// Total tokens (input + output).
    pub total_tokens: u64,
}

/// The result produced by running a scenario through the evaluation runner.
///
/// Captures token usage, latency, key output features, and a numeric score
/// to enable time-series drift detection.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScenarioResult {
    /// Identifier of the scenario that was evaluated.
    pub scenario_id: String,
    /// Token usage during this evaluation run.
    pub token_usage: TokenUsage,
    /// End-to-end latency of the evaluation in milliseconds.
    pub latency_ms: u64,
    /// Key output features as freeform JSON (model-specific metadata).
    pub output_features: serde_json::Value,
    /// Numeric score from the evaluation (0.0–1.0).
    pub score: f64,
    /// Human-readable reasoning accompanying the score.
    pub reasoning: String,
}

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
