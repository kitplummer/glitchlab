use serde::{Deserialize, Serialize};

use crate::agent::{AgentContext, AgentOutput};
use crate::budget::BudgetSummary;
use crate::error;
use crate::outcome::OutcomeContext;

// ---------------------------------------------------------------------------
// Pipeline trait
// ---------------------------------------------------------------------------

/// A pipeline defines how agents are orchestrated for a task type.
///
/// The engineering org uses a linear pipeline:
///   Plan → Implement → Test/Debug → Security → Release → Archive → PR
///
/// Other orgs may use event-driven, periodic, or ad-hoc pipelines.
pub trait Pipeline: Send + Sync {
    /// Execute the pipeline for a given task context.
    fn execute(
        &self,
        ctx: &mut PipelineContext,
    ) -> impl Future<Output = error::Result<PipelineResult>> + Send;
}

// ---------------------------------------------------------------------------
// PipelineContext — shared mutable state across pipeline stages
// ---------------------------------------------------------------------------

/// Mutable state that flows through the pipeline stages.
///
/// Each stage reads from `agent_context` and writes to `stage_outputs`.
/// The pipeline runner updates `current_stage` as it progresses.
pub struct PipelineContext {
    /// The agent context (updated between stages with previous output).
    pub agent_context: AgentContext,

    /// Outputs from completed stages, keyed by stage name.
    pub stage_outputs: std::collections::HashMap<String, AgentOutput>,

    /// Events emitted during pipeline execution.
    pub events: Vec<PipelineEvent>,

    /// The stage currently executing.
    pub current_stage: Option<String>,
}

impl PipelineContext {
    pub fn new(agent_context: AgentContext) -> Self {
        Self {
            agent_context,
            stage_outputs: std::collections::HashMap::new(),
            events: Vec::new(),
            current_stage: None,
        }
    }

    /// Record an event during pipeline execution.
    pub fn emit(&mut self, event: PipelineEvent) {
        self.events.push(event);
    }
}

// ---------------------------------------------------------------------------
// PipelineStage — a named unit of work in a pipeline
// ---------------------------------------------------------------------------

/// Defines a stage in a pipeline.
///
/// Stages are declarative — they describe what agent to invoke and
/// under what conditions. The pipeline runner handles the mechanics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStage {
    /// Unique name for this stage (e.g. "plan", "implement", "test").
    pub name: String,

    /// The agent role to invoke (must match an agent in the org).
    pub agent_role: String,

    /// Whether this stage can be skipped.
    #[serde(default)]
    pub optional: bool,

    /// Condition for running this stage.
    /// `None` means always run.
    #[serde(default)]
    pub condition: Option<StageCondition>,
}

/// Conditions under which a pipeline stage runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StageCondition {
    /// Run only if a previous stage produced a specific field value.
    PreviousOutput {
        stage: String,
        field: String,
        equals: serde_json::Value,
    },
    /// Run only if the task risk level matches.
    RiskLevel(String),
    /// Always skip (useful for disabling stages in config).
    Never,
}

// ---------------------------------------------------------------------------
// PipelineResult — outcome of a full pipeline run
// ---------------------------------------------------------------------------

/// The result of executing a complete pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineResult {
    /// Overall status.
    pub status: PipelineStatus,

    /// Output from each stage that ran.
    pub stage_outputs: std::collections::HashMap<String, AgentOutput>,

    /// Events emitted during execution.
    pub events: Vec<PipelineEvent>,

    /// Budget summary at completion.
    pub budget: BudgetSummary,

    /// If a PR was created, its URL.
    pub pr_url: Option<String>,

    /// The branch name (even if PR wasn't created).
    pub branch: Option<String>,

    /// Error message if the pipeline failed.
    pub error: Option<String>,

    /// Structured context from a failed/deferred run (for learning).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_context: Option<OutcomeContext>,
}

/// Possible pipeline outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStatus {
    /// Pipeline completed successfully and a PR was created.
    PrCreated,
    /// Pipeline completed but stopped before PR (committed locally).
    Committed,
    /// Planning stage failed.
    PlanFailed,
    /// Implementation stage failed.
    ImplementationFailed,
    /// Tests failed after all debug attempts.
    TestsFailed,
    /// Security review blocked the changes.
    SecurityBlocked,
    /// A boundary violation was detected.
    BoundaryViolation,
    /// Budget was exceeded.
    BudgetExceeded,
    /// Pipeline was interrupted (human intervention).
    Interrupted,
    /// Pipeline exceeded its wall-clock timeout.
    TimedOut,
    /// Task was decomposed into sub-tasks (not a failure).
    Decomposed,
    /// Agent couldn't proceed — re-queue with context for a later attempt.
    Deferred,
    /// External dependency or human decision needed — not retryable automatically.
    Blocked,
    /// Transient failure — safe to retry with backoff.
    Retryable,
    /// Unrecoverable error.
    Error,
}

// ---------------------------------------------------------------------------
// PipelineEvent — structured event log
// ---------------------------------------------------------------------------

/// An event emitted during pipeline execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineEvent {
    /// Event type.
    pub kind: EventKind,

    /// ISO 8601 timestamp.
    pub timestamp: String,

    /// Task ID this event belongs to.
    pub task_id: String,

    /// Event-specific data.
    #[serde(default)]
    pub data: serde_json::Value,
}

/// Types of pipeline events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    WorkspaceCreated,
    PlanCreated,
    BoundaryChecked,
    ImplementationComplete,
    TestsPassed,
    TestsFailed,
    DebugAttempt,
    SecurityReview,
    ReleaseAssessment,
    DocumentationWritten,
    Committed,
    PrCreated,
    BudgetExceeded,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentContext;
    use std::collections::HashMap;

    fn test_agent_context() -> AgentContext {
        AgentContext {
            task_id: "test-1".into(),
            objective: "Test objective".into(),
            repo_path: "/tmp/repo".into(),
            working_dir: "/tmp/repo/wt".into(),
            constraints: vec![],
            acceptance_criteria: vec![],
            risk_level: "low".into(),
            file_context: HashMap::new(),
            previous_output: serde_json::Value::Null,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn pipeline_context_new() {
        let ctx = PipelineContext::new(test_agent_context());
        assert!(ctx.stage_outputs.is_empty());
        assert!(ctx.events.is_empty());
        assert!(ctx.current_stage.is_none());
    }

    #[test]
    fn pipeline_context_emit() {
        let mut ctx = PipelineContext::new(test_agent_context());
        ctx.emit(PipelineEvent {
            kind: EventKind::PlanCreated,
            timestamp: "2025-01-01T00:00:00Z".into(),
            task_id: "test-1".into(),
            data: serde_json::Value::Null,
        });
        assert_eq!(ctx.events.len(), 1);
        assert_eq!(ctx.events[0].kind, EventKind::PlanCreated);
    }

    #[test]
    fn pipeline_status_serde() {
        let status = PipelineStatus::PrCreated;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"pr_created\"");
        let parsed: PipelineStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, PipelineStatus::PrCreated);
    }

    #[test]
    fn pipeline_status_timed_out_serde() {
        let status = PipelineStatus::TimedOut;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"timed_out\"");
        let parsed: PipelineStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, PipelineStatus::TimedOut);
    }

    #[test]
    fn event_kind_serde() {
        let kind = EventKind::TestsFailed;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"tests_failed\"");
    }

    #[test]
    fn stage_condition_serde() {
        let cond = StageCondition::RiskLevel("high".into());
        let json = serde_json::to_string(&cond).unwrap();
        let parsed: StageCondition = serde_json::from_str(&json).unwrap();
        match parsed {
            StageCondition::RiskLevel(level) => assert_eq!(level, "high"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pipeline_stage_optional_default() {
        let json = r#"{"name": "plan", "agent_role": "planner"}"#;
        let stage: PipelineStage = serde_json::from_str(json).unwrap();
        assert!(!stage.optional);
        assert!(stage.condition.is_none());
    }

    #[test]
    fn pipeline_status_deferred_serde() {
        let status = PipelineStatus::Deferred;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"deferred\"");
        let parsed: PipelineStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, PipelineStatus::Deferred);
    }

    #[test]
    fn pipeline_status_blocked_serde() {
        let status = PipelineStatus::Blocked;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"blocked\"");
        let parsed: PipelineStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, PipelineStatus::Blocked);
    }

    #[test]
    fn pipeline_status_retryable_serde() {
        let status = PipelineStatus::Retryable;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"retryable\"");
        let parsed: PipelineStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, PipelineStatus::Retryable);
    }

    #[test]
    fn pipeline_result_with_outcome_context() {
        use crate::outcome::{ObstacleKind, OutcomeContext};

        let result = PipelineResult {
            status: PipelineStatus::Deferred,
            stage_outputs: HashMap::new(),
            events: vec![],
            budget: crate::budget::BudgetSummary {
                total_tokens: 100,
                estimated_cost: 0.01,
                call_count: 1,
                tokens_remaining: 99900,
                dollars_remaining: 9.99,
            },
            pr_url: None,
            branch: None,
            error: Some("deferred: missing prerequisite".into()),
            outcome_context: Some(OutcomeContext {
                approach: "tried to implement".into(),
                obstacle: ObstacleKind::MissingPrerequisite {
                    task_id: "dep-1".into(),
                    reason: "schema not ready".into(),
                },
                discoveries: vec!["found existing migration".into()],
                recommendation: Some("wait for dep-1".into()),
                files_explored: vec!["src/db.rs".into()],
            }),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("outcome_context"));
        assert!(json.contains("missing_prerequisite"));
        let parsed: PipelineResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.outcome_context.is_some());
    }

    #[test]
    fn pipeline_result_without_outcome_context() {
        let result = PipelineResult {
            status: PipelineStatus::PrCreated,
            stage_outputs: HashMap::new(),
            events: vec![],
            budget: crate::budget::BudgetSummary {
                total_tokens: 100,
                estimated_cost: 0.01,
                call_count: 1,
                tokens_remaining: 99900,
                dollars_remaining: 9.99,
            },
            pr_url: Some("https://example.com/pr/1".into()),
            branch: Some("feature/x".into()),
            error: None,
            outcome_context: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        // outcome_context should be skipped when None
        assert!(!json.contains("outcome_context"));
    }
}
