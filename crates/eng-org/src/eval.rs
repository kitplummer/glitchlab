use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EvaluationScenario {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EvaluationResult {
    pub score: f64,
    pub reasoning: String,
}

/// Evaluates a scenario.
///
/// This is a placeholder and will be implemented later.
pub fn evaluate_scenario(_scenario: &EvaluationScenario) -> EvaluationResult {
    // unimplemented!("evaluate_scenario is not yet implemented");
    EvaluationResult {
        score: 0.0,
        reasoning: "Not implemented".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // #[should_panic(expected = "evaluate_scenario is not yet implemented")]
    fn test_evaluate_scenario_panics() {
        let scenario = EvaluationScenario {
            name: "test".to_string(),
            description: "a test scenario".to_string(),
        };
        let result = evaluate_scenario(&scenario);
        assert_eq!(result.score, 0.0);
        assert_eq!(result.reasoning, "Not implemented".to_string());
    }
}
