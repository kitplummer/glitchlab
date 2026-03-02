use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

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
    /// Work already done — triage detected no implementation needed.
    AlreadyDone,
    /// LLM returned unparseable output — retryable with model escalation.
    ParseError,
    /// Architect review rejected the changes.
    ArchitectRejected,
    /// PR was created and merged automatically.
    PrMerged,
    /// Human decision or external input required — not retryable automatically.
    Escalated,
    /// Unrecoverable error.
    Error,
}

impl std::fmt::Display for PipelineStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_string(self).map_err(|_| std::fmt::Error)?;
        // serde_json adds quotes, remove them
        f.write_str(&s.replace('"', ""))
    }
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
    CisoReview,
    ReleaseAssessment,
    DocumentationWritten,
    Committed,
    PrCreated,
    ArchitectTriage,
    ArchitectReview,
    ArchitectPrReview,
    PrMerged,
    BudgetExceeded,
    Error,
}

// ---------------------------------------------------------------------------
// StageRunner — object-safe agent invocation for LinearPipeline
// ---------------------------------------------------------------------------

/// Object-safe callable that executes a named agent role within a pipeline stage.
///
/// This trait bridges the gap between the RPIT-based [`Agent`] trait (which is
/// not object-safe) and the needs of [`LinearPipeline`], which must dispatch to
/// agents by role name at runtime.
///
/// [`Agent`]: crate::agent::Agent
pub trait StageRunner: Send + Sync {
    /// Run the agent with the given role, returning its output.
    fn run_stage<'a>(
        &'a self,
        role: &'a str,
        ctx: &'a AgentContext,
    ) -> Pin<Box<dyn Future<Output = error::Result<AgentOutput>> + Send + 'a>>;
}

// ---------------------------------------------------------------------------
// LinearPipeline — sequential stage execution
// ---------------------------------------------------------------------------

/// A pipeline that executes stages sequentially, in declaration order.
///
/// Each stage is checked for its run condition before execution.
/// Required stages that fail terminate the pipeline with [`PipelineStatus::Error`].
/// Optional stages that fail are silently skipped and execution continues.
pub struct LinearPipeline {
    stages: Vec<PipelineStage>,
    runner: Arc<dyn StageRunner>,
}

impl LinearPipeline {
    /// Create a new `LinearPipeline` with the given stages and runner.
    pub fn new(stages: Vec<PipelineStage>, runner: Arc<dyn StageRunner>) -> Self {
        Self { stages, runner }
    }

    fn should_run_stage(&self, stage: &PipelineStage, ctx: &PipelineContext) -> bool {
        match &stage.condition {
            None => true,
            Some(StageCondition::Never) => false,
            Some(StageCondition::RiskLevel(required)) => &ctx.agent_context.risk_level == required,
            Some(StageCondition::PreviousOutput {
                stage: prev_stage,
                field,
                equals,
            }) => ctx
                .stage_outputs
                .get(prev_stage)
                .and_then(|output| output.data.get(field))
                .map(|v| v == equals)
                .unwrap_or(false),
        }
    }
}

impl Pipeline for LinearPipeline {
    async fn execute(&self, ctx: &mut PipelineContext) -> error::Result<PipelineResult> {
        for stage in &self.stages {
            if !self.should_run_stage(stage, ctx) {
                continue;
            }

            ctx.current_stage = Some(stage.name.clone());

            match self
                .runner
                .run_stage(&stage.agent_role, &ctx.agent_context)
                .await
            {
                Ok(output) => {
                    ctx.agent_context.previous_output = output.data.clone();
                    ctx.stage_outputs.insert(stage.name.clone(), output);
                }
                Err(e) => {
                    if stage.optional {
                        continue;
                    }
                    ctx.current_stage = None;
                    return Ok(PipelineResult {
                        status: PipelineStatus::Error,
                        stage_outputs: ctx.stage_outputs.clone(),
                        events: ctx.events.clone(),
                        budget: BudgetSummary::default(),
                        pr_url: None,
                        branch: None,
                        error: Some(e.to_string()),
                        outcome_context: None,
                    });
                }
            }
        }

        ctx.current_stage = None;
        Ok(PipelineResult {
            status: PipelineStatus::Committed,
            stage_outputs: ctx.stage_outputs.clone(),
            events: ctx.events.clone(),
            budget: BudgetSummary::default(),
            pr_url: None,
            branch: None,
            error: None,
            outcome_context: None,
        })
    }
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
    fn pipeline_status_already_done_serde() {
        let status = PipelineStatus::AlreadyDone;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"already_done\"");
        let parsed: PipelineStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, PipelineStatus::AlreadyDone);
    }

    #[test]
    fn pipeline_status_architect_rejected_serde() {
        let status = PipelineStatus::ArchitectRejected;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"architect_rejected\"");
        let parsed: PipelineStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, PipelineStatus::ArchitectRejected);
    }

    #[test]
    fn pipeline_status_pr_merged_serde() {
        let status = PipelineStatus::PrMerged;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"pr_merged\"");
        let parsed: PipelineStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, PipelineStatus::PrMerged);
    }

    #[test]
    fn pipeline_status_parse_error_serde() {
        let status = PipelineStatus::ParseError;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"parse_error\"");
        let parsed: PipelineStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, PipelineStatus::ParseError);
    }

    #[test]
    fn pipeline_status_escalated_serde() {
        let status = PipelineStatus::Escalated;
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, "\"escalated\"");
        let parsed: PipelineStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, PipelineStatus::Escalated);
    }

    #[test]
    fn event_kind_architect_triage_serde() {
        let kind = EventKind::ArchitectTriage;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"architect_triage\"");
    }

    #[test]
    fn event_kind_architect_review_serde() {
        let kind = EventKind::ArchitectReview;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"architect_review\"");
    }

    #[test]
    fn event_kind_architect_pr_review_serde() {
        let kind = EventKind::ArchitectPrReview;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"architect_pr_review\"");
    }

    #[test]
    fn event_kind_pr_merged_serde() {
        let kind = EventKind::PrMerged;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"pr_merged\"");
    }

    #[test]
    fn pipeline_status_display() {
        assert_eq!(PipelineStatus::PrCreated.to_string(), "pr_created");
        assert_eq!(PipelineStatus::Committed.to_string(), "committed");
        assert_eq!(PipelineStatus::PlanFailed.to_string(), "plan_failed");
        assert_eq!(
            PipelineStatus::ImplementationFailed.to_string(),
            "implementation_failed"
        );
        assert_eq!(PipelineStatus::TestsFailed.to_string(), "tests_failed");
        assert_eq!(
            PipelineStatus::SecurityBlocked.to_string(),
            "security_blocked"
        );
        assert_eq!(
            PipelineStatus::BoundaryViolation.to_string(),
            "boundary_violation"
        );
        assert_eq!(
            PipelineStatus::BudgetExceeded.to_string(),
            "budget_exceeded"
        );
        assert_eq!(PipelineStatus::Interrupted.to_string(), "interrupted");
        assert_eq!(PipelineStatus::TimedOut.to_string(), "timed_out");
        assert_eq!(PipelineStatus::Decomposed.to_string(), "decomposed");
        assert_eq!(PipelineStatus::Deferred.to_string(), "deferred");
        assert_eq!(PipelineStatus::Blocked.to_string(), "blocked");
        assert_eq!(PipelineStatus::Retryable.to_string(), "retryable");
        assert_eq!(PipelineStatus::AlreadyDone.to_string(), "already_done");
        assert_eq!(PipelineStatus::ParseError.to_string(), "parse_error");
        assert_eq!(
            PipelineStatus::ArchitectRejected.to_string(),
            "architect_rejected"
        );
        assert_eq!(PipelineStatus::PrMerged.to_string(), "pr_merged");
        assert_eq!(PipelineStatus::Escalated.to_string(), "escalated");
        assert_eq!(PipelineStatus::Error.to_string(), "error");
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

    // -----------------------------------------------------------------------
    // LinearPipeline tests
    // -----------------------------------------------------------------------

    use crate::agent::{AgentMetadata, AgentOutput};
    use crate::error;
    use std::collections::HashSet;
    use std::pin::Pin;
    use std::sync::Arc;

    fn test_output() -> AgentOutput {
        AgentOutput {
            data: serde_json::json!({"result": "ok"}),
            metadata: AgentMetadata {
                agent: "mock".into(),
                model: "mock-model".into(),
                tokens: 10,
                cost: 0.001,
                latency_ms: 50,
            },
            parse_error: false,
        }
    }

    struct MockRunner {
        fail_roles: HashSet<String>,
    }

    impl StageRunner for MockRunner {
        fn run_stage<'a>(
            &'a self,
            role: &'a str,
            _ctx: &'a crate::agent::AgentContext,
        ) -> Pin<Box<dyn std::future::Future<Output = error::Result<AgentOutput>> + Send + 'a>>
        {
            if self.fail_roles.contains(role) {
                Box::pin(async move {
                    Err(error::Error::Agent {
                        agent: role.to_string(),
                        reason: "mock failure".into(),
                    })
                })
            } else {
                Box::pin(async { Ok(test_output()) })
            }
        }
    }

    fn mock_runner(fail_roles: &[&str]) -> Arc<MockRunner> {
        Arc::new(MockRunner {
            fail_roles: fail_roles.iter().map(|s| s.to_string()).collect(),
        })
    }

    fn stage(
        name: &str,
        role: &str,
        optional: bool,
        condition: Option<StageCondition>,
    ) -> PipelineStage {
        PipelineStage {
            name: name.into(),
            agent_role: role.into(),
            optional,
            condition,
        }
    }

    #[tokio::test]
    async fn linear_pipeline_empty_stages_returns_committed() {
        let pipeline = LinearPipeline::new(vec![], mock_runner(&[]));
        let mut ctx = PipelineContext::new(test_agent_context());
        let result = pipeline.execute(&mut ctx).await.unwrap();
        assert_eq!(result.status, PipelineStatus::Committed);
        assert!(result.stage_outputs.is_empty());
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn linear_pipeline_runs_single_stage() {
        let stages = vec![stage("plan", "planner", false, None)];
        let pipeline = LinearPipeline::new(stages, mock_runner(&[]));
        let mut ctx = PipelineContext::new(test_agent_context());
        let result = pipeline.execute(&mut ctx).await.unwrap();
        assert_eq!(result.status, PipelineStatus::Committed);
        assert!(result.stage_outputs.contains_key("plan"));
    }

    #[tokio::test]
    async fn linear_pipeline_runs_multiple_stages_in_order() {
        let stages = vec![
            stage("plan", "planner", false, None),
            stage("implement", "implementer", false, None),
        ];
        let pipeline = LinearPipeline::new(stages, mock_runner(&[]));
        let mut ctx = PipelineContext::new(test_agent_context());
        let result = pipeline.execute(&mut ctx).await.unwrap();
        assert_eq!(result.status, PipelineStatus::Committed);
        assert!(result.stage_outputs.contains_key("plan"));
        assert!(result.stage_outputs.contains_key("implement"));
    }

    #[tokio::test]
    async fn linear_pipeline_required_stage_failure_returns_error() {
        let stages = vec![stage("plan", "planner", false, None)];
        let pipeline = LinearPipeline::new(stages, mock_runner(&["planner"]));
        let mut ctx = PipelineContext::new(test_agent_context());
        let result = pipeline.execute(&mut ctx).await.unwrap();
        assert_eq!(result.status, PipelineStatus::Error);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn linear_pipeline_optional_stage_failure_continues() {
        let stages = vec![
            stage("plan", "planner", true, None), // optional, will fail
            stage("implement", "implementer", false, None), // required, succeeds
        ];
        let pipeline = LinearPipeline::new(stages, mock_runner(&["planner"]));
        let mut ctx = PipelineContext::new(test_agent_context());
        let result = pipeline.execute(&mut ctx).await.unwrap();
        assert_eq!(result.status, PipelineStatus::Committed);
        // optional stage skipped, required stage ran
        assert!(!result.stage_outputs.contains_key("plan"));
        assert!(result.stage_outputs.contains_key("implement"));
    }

    #[tokio::test]
    async fn linear_pipeline_never_condition_skips_stage() {
        let stages = vec![stage("plan", "planner", false, Some(StageCondition::Never))];
        // runner would fail if called, but Never condition means it won't be
        let pipeline = LinearPipeline::new(stages, mock_runner(&["planner"]));
        let mut ctx = PipelineContext::new(test_agent_context());
        let result = pipeline.execute(&mut ctx).await.unwrap();
        assert_eq!(result.status, PipelineStatus::Committed);
        assert!(result.stage_outputs.is_empty());
    }

    #[tokio::test]
    async fn linear_pipeline_risk_condition_matches() {
        let stages = vec![stage(
            "security",
            "security",
            false,
            Some(StageCondition::RiskLevel("high".into())),
        )];
        let pipeline = LinearPipeline::new(stages, mock_runner(&[]));
        let mut ctx = PipelineContext::new(test_agent_context());
        // default risk is "low", so stage should be skipped
        let result = pipeline.execute(&mut ctx).await.unwrap();
        assert_eq!(result.status, PipelineStatus::Committed);
        assert!(!result.stage_outputs.contains_key("security"));
    }

    #[tokio::test]
    async fn linear_pipeline_risk_condition_matches_high() {
        let stages = vec![stage(
            "security",
            "security",
            false,
            Some(StageCondition::RiskLevel("high".into())),
        )];
        let pipeline = LinearPipeline::new(stages, mock_runner(&[]));
        let mut ctx = PipelineContext::new(test_agent_context());
        ctx.agent_context.risk_level = "high".into();
        let result = pipeline.execute(&mut ctx).await.unwrap();
        assert_eq!(result.status, PipelineStatus::Committed);
        assert!(result.stage_outputs.contains_key("security"));
    }

    #[tokio::test]
    async fn linear_pipeline_previous_output_condition() {
        let stages = vec![
            stage("plan", "planner", false, None),
            stage(
                "extra",
                "extra-agent",
                false,
                Some(StageCondition::PreviousOutput {
                    stage: "plan".into(),
                    field: "result".into(),
                    equals: serde_json::json!("ok"),
                }),
            ),
        ];
        let pipeline = LinearPipeline::new(stages, mock_runner(&[]));
        let mut ctx = PipelineContext::new(test_agent_context());
        let result = pipeline.execute(&mut ctx).await.unwrap();
        // plan output is {"result": "ok"}, so extra stage should run
        assert_eq!(result.status, PipelineStatus::Committed);
        assert!(result.stage_outputs.contains_key("extra"));
    }

    #[tokio::test]
    async fn linear_pipeline_updates_previous_output_in_context() {
        let stages = vec![
            stage("plan", "planner", false, None),
            stage("implement", "implementer", false, None),
        ];
        let pipeline = LinearPipeline::new(stages, mock_runner(&[]));
        let mut ctx = PipelineContext::new(test_agent_context());
        pipeline.execute(&mut ctx).await.unwrap();
        // After execution, previous_output should be set from last stage
        assert_ne!(ctx.agent_context.previous_output, serde_json::Value::Null);
    }

    #[tokio::test]
    async fn linear_pipeline_current_stage_cleared_after_run() {
        let stages = vec![stage("plan", "planner", false, None)];
        let pipeline = LinearPipeline::new(stages, mock_runner(&[]));
        let mut ctx = PipelineContext::new(test_agent_context());
        pipeline.execute(&mut ctx).await.unwrap();
        assert!(ctx.current_stage.is_none());
    }
}
