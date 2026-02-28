use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Outcome — final result of a pipeline run
// ---------------------------------------------------------------------------

/// The final result of a pipeline run, capturing either success or failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    /// The pipeline completed its objective successfully.
    Completed(CompletionContext),
    /// The pipeline failed to complete its objective.
    Failed(OutcomeContext),
}

// ---------------------------------------------------------------------------
// CompletionContext — structured context from a successful pipeline run
// ---------------------------------------------------------------------------

/// Structured context captured when a pipeline run succeeds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionContext {
    /// A concise, one-sentence summary of what was accomplished.
    pub summary: String,

    /// Files that were created or modified during the pipeline run.
    #[serde(default)]
    pub files_changed: Vec<String>,

    /// New tests that were added.
    #[serde(default)]
    pub tests_added: Vec<String>,
}

// ---------------------------------------------------------------------------
// OutcomeContext — structured context from a pipeline failure
// ---------------------------------------------------------------------------

/// Structured context captured when a pipeline run fails or is deferred.
///
/// Preserves the approach taken, the obstacle encountered, and any
/// discoveries made so that subsequent attempts can learn from the failure
/// rather than repeating the same dead-end.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeContext {
    /// What approach the agent took (e.g. "modified Cargo.toml to add uuid").
    pub approach: String,

    /// Categorised obstacle that prevented completion.
    pub obstacle: ObstacleKind,

    /// Things the agent discovered that might help next time.
    #[serde(default)]
    pub discoveries: Vec<String>,

    /// Agent's recommendation for the next attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,

    /// Files the agent explored or modified.
    #[serde(default)]
    pub files_explored: Vec<String>,
}

// ---------------------------------------------------------------------------
// ObstacleKind — taxonomy of failure reasons
// ---------------------------------------------------------------------------

/// Categorised obstacle that prevented a pipeline from completing.
///
/// Tagged-enum serialisation (`kind` field) keeps history JSONL human-readable
/// and lets the planner pattern-match on obstacle types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObstacleKind {
    /// A prerequisite task must complete first.
    MissingPrerequisite { task_id: String, reason: String },

    /// The codebase architecture doesn't support the requested change.
    ArchitecturalGap { description: String },

    /// The LLM couldn't produce a valid response.
    ModelLimitation { model: String, error_class: String },

    /// An external service or tool was unavailable.
    ExternalDependency { service: String },

    /// The task touches too many files for a single pipeline run.
    ScopeTooLarge {
        estimated_files: usize,
        max_files: usize,
    },

    /// Tests failed after repeated debug attempts.
    TestFailure { attempts: u32, last_error: String },

    /// LLM output couldn't be parsed into the expected schema.
    ParseFailure { model: String, raw_snippet: String },

    /// LLM output is valid JSON but doesn't match the expected schema.
    SchemaMismatch {
        model: String,
        expected_schema: String,
        validation_error: String,
    },

    /// Catch-all for unclassified obstacles.
    Unknown { detail: String },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T: Serialize + for<'de> Deserialize<'de>>(val: &T) -> T {
        let json = serde_json::to_string(val).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn outcome_completed_serde() {
        let outcome = Outcome::Completed(CompletionContext {
            summary: "Implemented the feature".into(),
            files_changed: vec!["src/main.rs".into()],
            tests_added: vec!["tests/test_main.rs".into()],
        });
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("\"status\":\"completed\""));
        assert!(json.contains("\"summary\":\"Implemented the feature\""));
        let parsed: Outcome = serde_json::from_str(&json).unwrap();
        match parsed {
            Outcome::Completed(ctx) => {
                assert_eq!(ctx.summary, "Implemented the feature");
                assert_eq!(ctx.files_changed, vec!["src/main.rs"]);
                assert_eq!(ctx.tests_added, vec!["tests/test_main.rs"]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn outcome_failed_serde() {
        let outcome = Outcome::Failed(OutcomeContext {
            approach: "Tried to implement".into(),
            obstacle: ObstacleKind::TestFailure {
                attempts: 3,
                last_error: "assertion failed".into(),
            },
            discoveries: vec![],
            recommendation: None,
            files_explored: vec!["src/main.rs".into()],
        });
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(json.contains("\"status\":\"failed\""));
        assert!(json.contains("\"approach\":\"Tried to implement\""));
        let parsed: Outcome = serde_json::from_str(&json).unwrap();
        match parsed {
            Outcome::Failed(ctx) => {
                assert_eq!(ctx.approach, "Tried to implement");
                assert_eq!(ctx.files_explored, vec!["src/main.rs"]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn completion_context_serde() {
        let ctx = CompletionContext {
            summary: "Fixed a bug".into(),
            files_changed: vec!["a.txt".into()],
            tests_added: vec![],
        };
        let parsed = roundtrip(&ctx);
        assert_eq!(parsed.summary, "Fixed a bug");
        assert_eq!(parsed.files_changed, vec!["a.txt"]);
        assert!(parsed.tests_added.is_empty());
    }

    #[test]
    fn obstacle_missing_prerequisite_serde() {
        let o = ObstacleKind::MissingPrerequisite {
            task_id: "dep-1".into(),
            reason: "needs schema migration".into(),
        };
        let json = serde_json::to_string(&o).unwrap();
        assert!(json.contains("\"kind\":\"missing_prerequisite\""));
        let parsed: ObstacleKind = serde_json::from_str(&json).unwrap();
        match parsed {
            ObstacleKind::MissingPrerequisite { task_id, reason } => {
                assert_eq!(task_id, "dep-1");
                assert_eq!(reason, "needs schema migration");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn obstacle_architectural_gap_serde() {
        let o = ObstacleKind::ArchitecturalGap {
            description: "no plugin system".into(),
        };
        let parsed = roundtrip(&o);
        match parsed {
            ObstacleKind::ArchitecturalGap { description } => {
                assert_eq!(description, "no plugin system");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn obstacle_model_limitation_serde() {
        let o = ObstacleKind::ModelLimitation {
            model: "gpt-4".into(),
            error_class: "context_window_exceeded".into(),
        };
        let parsed = roundtrip(&o);
        match parsed {
            ObstacleKind::ModelLimitation { model, error_class } => {
                assert_eq!(model, "gpt-4");
                assert_eq!(error_class, "context_window_exceeded");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn obstacle_external_dependency_serde() {
        let o = ObstacleKind::ExternalDependency {
            service: "npm registry".into(),
        };
        let parsed = roundtrip(&o);
        match parsed {
            ObstacleKind::ExternalDependency { service } => {
                assert_eq!(service, "npm registry");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn obstacle_scope_too_large_serde() {
        let o = ObstacleKind::ScopeTooLarge {
            estimated_files: 42,
            max_files: 20,
        };
        let parsed = roundtrip(&o);
        match parsed {
            ObstacleKind::ScopeTooLarge {
                estimated_files,
                max_files,
            } => {
                assert_eq!(estimated_files, 42);
                assert_eq!(max_files, 20);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn obstacle_test_failure_serde() {
        let o = ObstacleKind::TestFailure {
            attempts: 3,
            last_error: "assertion failed".into(),
        };
        let parsed = roundtrip(&o);
        match parsed {
            ObstacleKind::TestFailure {
                attempts,
                last_error,
            } => {
                assert_eq!(attempts, 3);
                assert_eq!(last_error, "assertion failed");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn obstacle_parse_failure_serde() {
        let o = ObstacleKind::ParseFailure {
            model: "claude-3".into(),
            raw_snippet: "not json {".into(),
        };
        let parsed = roundtrip(&o);
        match parsed {
            ObstacleKind::ParseFailure { model, raw_snippet } => {
                assert_eq!(model, "claude-3");
                assert_eq!(raw_snippet, "not json {");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn obstacle_schema_mismatch_serde() {
        let o = ObstacleKind::SchemaMismatch {
            model: "claude-3".into(),
            expected_schema: "TaskPlan".into(),
            validation_error: "missing required field 'steps'".into(),
        };
        let parsed = roundtrip(&o);
        match parsed {
            ObstacleKind::SchemaMismatch {
                model,
                expected_schema,
                validation_error,
            } => {
                assert_eq!(model, "claude-3");
                assert_eq!(expected_schema, "TaskPlan");
                assert_eq!(validation_error, "missing required field 'steps'");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn obstacle_unknown_serde() {
        let o = ObstacleKind::Unknown {
            detail: "something unexpected".into(),
        };
        let parsed = roundtrip(&o);
        match parsed {
            ObstacleKind::Unknown { detail } => {
                assert_eq!(detail, "something unexpected");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn outcome_context_full_roundtrip() {
        let ctx = OutcomeContext {
            approach: "added greet() to lib.rs".into(),
            obstacle: ObstacleKind::TestFailure {
                attempts: 2,
                last_error: "test_greet failed".into(),
            },
            discoveries: vec!["lib.rs uses no_std".into(), "greet is re-exported".into()],
            recommendation: Some("use core::fmt instead of std::fmt".into()),
            files_explored: vec!["src/lib.rs".into(), "Cargo.toml".into()],
        };
        let json = serde_json::to_string_pretty(&ctx).unwrap();
        let parsed: OutcomeContext = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.approach, ctx.approach);
        assert_eq!(parsed.discoveries.len(), 2);
        assert!(parsed.recommendation.is_some());
        assert_eq!(parsed.files_explored.len(), 2);
    }

    #[test]
    fn outcome_context_minimal_roundtrip() {
        let ctx = OutcomeContext {
            approach: "tried something".into(),
            obstacle: ObstacleKind::Unknown {
                detail: "shrug".into(),
            },
            discoveries: vec![],
            recommendation: None,
            files_explored: vec![],
        };
        let json = serde_json::to_string(&ctx).unwrap();
        // recommendation should be skipped
        assert!(!json.contains("recommendation"));
        let parsed: OutcomeContext = serde_json::from_str(&json).unwrap();
        assert!(parsed.recommendation.is_none());
        assert!(parsed.discoveries.is_empty());
        assert!(parsed.files_explored.is_empty());
    }

    #[test]
    fn outcome_context_deserialize_missing_optional_fields() {
        // Simulate old/minimal JSON that lacks optional fields.
        let json = r#"{
            "approach": "manual edit",
            "obstacle": {"kind": "unknown", "detail": "?"}
        }"#;
        let parsed: OutcomeContext = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.approach, "manual edit");
        assert!(parsed.discoveries.is_empty());
        assert!(parsed.recommendation.is_none());
        assert!(parsed.files_explored.is_empty());
    }
}
