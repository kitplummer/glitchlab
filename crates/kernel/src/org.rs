use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::budget::BudgetTracker;
use crate::governance::ZephyrPolicy;
use crate::pipeline::PipelineStage;
use crate::tool::{ToolDefinition, ToolPolicy};

// ---------------------------------------------------------------------------
// OrgConfig — deserialized org configuration
// ---------------------------------------------------------------------------

/// Configuration for an org, loaded from YAML/TOML.
///
/// This is the data side of the Org construct defined in the
/// corporation framework ADR. It describes what an org is;
/// the Org trait (below) describes what it can do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrgConfig {
    /// Org name (e.g. "engineering", "operations", "finance").
    pub name: String,

    /// Agent configurations keyed by role.
    pub agents: HashMap<String, AgentConfig>,

    /// Tool policy for this org.
    pub tools: ToolPolicy,

    /// Pipeline stages (ordered for linear pipelines).
    pub pipeline: Vec<PipelineStage>,

    /// Governance policy.
    pub governance: ZephyrPolicy,

    /// Model routing: agent role → model string.
    pub routing: HashMap<String, String>,

    /// Budget limits.
    pub limits: LimitsConfig,

    /// Human intervention gates.
    #[serde(default)]
    pub intervention: InterventionConfig,
}

/// Configuration for a single agent within an org.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Agent role identifier.
    pub role: String,

    /// Human-readable persona name.
    pub persona: String,

    /// Path to the system prompt template, or inline prompt.
    pub system_prompt: String,

    /// Default model override (if different from routing config).
    #[serde(default)]
    pub model: Option<String>,

    /// Maximum tokens for this agent's LLM call.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,

    /// Temperature for this agent's LLM call.
    #[serde(default = "default_temperature")]
    pub temperature: f32,
}

fn default_max_tokens() -> u32 {
    4096
}

fn default_temperature() -> f32 {
    0.2
}

/// Budget limits for a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    /// Maximum fix/debug loop iterations.
    #[serde(default = "default_max_fix_attempts")]
    pub max_fix_attempts: u32,

    /// Maximum tokens per task across all agent calls.
    #[serde(default = "default_max_tokens_per_task")]
    pub max_tokens_per_task: u64,

    /// Maximum dollar spend per task.
    #[serde(default = "default_max_dollars")]
    pub max_dollars_per_task: f64,

    /// Require human review of the plan before implementation.
    #[serde(default = "yes")]
    pub require_plan_review: bool,

    /// Require human review before creating PR.
    #[serde(default = "yes")]
    pub require_pr_review: bool,

    /// Maximum LLM round-trips per agent tool-use session.
    #[serde(default = "default_max_tool_turns")]
    pub max_tool_turns: u32,
}

fn default_max_fix_attempts() -> u32 {
    4
}
fn default_max_tokens_per_task() -> u64 {
    150_000
}
fn default_max_dollars() -> f64 {
    10.0
}
fn default_max_tool_turns() -> u32 {
    20
}
fn yes() -> bool {
    true
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_fix_attempts: default_max_fix_attempts(),
            max_tokens_per_task: default_max_tokens_per_task(),
            max_dollars_per_task: default_max_dollars(),
            require_plan_review: true,
            require_pr_review: true,
            max_tool_turns: default_max_tool_turns(),
        }
    }
}

/// Human intervention configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterventionConfig {
    /// Pause after planning for human review.
    #[serde(default = "yes")]
    pub pause_after_plan: bool,

    /// Pause before PR creation for human review.
    #[serde(default = "yes")]
    pub pause_before_pr: bool,

    /// Pause when protected paths are affected.
    #[serde(default = "yes")]
    pub pause_on_core_change: bool,

    /// Pause when budget is exceeded.
    #[serde(default = "yes")]
    pub pause_on_budget_exceeded: bool,
}

impl Default for InterventionConfig {
    fn default() -> Self {
        Self {
            pause_after_plan: true,
            pause_before_pr: true,
            pause_on_core_change: true,
            pause_on_budget_exceeded: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Task — a unit of work to be processed by an org
// ---------------------------------------------------------------------------

/// A task to be processed by an org's pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique task identifier.
    pub task_id: String,

    /// Full task description.
    pub objective: String,

    /// Task-level constraints.
    #[serde(default)]
    pub constraints: Vec<String>,

    /// Success criteria.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,

    /// Risk level: "low", "medium", "high".
    #[serde(default = "default_risk")]
    pub risk_level: String,

    /// Where this task came from: "github", "local", "interactive".
    #[serde(default = "default_source")]
    pub source: String,
}

fn default_risk() -> String {
    "low".into()
}
fn default_source() -> String {
    "local".into()
}

impl Task {
    /// Create a BudgetTracker from this org's limits config.
    pub fn budget_tracker(limits: &LimitsConfig) -> BudgetTracker {
        BudgetTracker::new(limits.max_tokens_per_task, limits.max_dollars_per_task)
    }
}

// ---------------------------------------------------------------------------
// OrgTrait — marker trait for organizational structures
// ---------------------------------------------------------------------------

/// Marker trait for organizational structures.
///
/// Implemented by any type that encapsulates the six core properties of the
/// corporation framework ADR: Agents, Tools, Leader, Pipeline, Governance,
/// and Memory.
pub trait OrgTrait: Send + Sync {}

// ---------------------------------------------------------------------------
// OrgStruct — runtime encapsulation of the six core ADR properties
// ---------------------------------------------------------------------------

/// Runtime org that encapsulates the six properties defined in the
/// corporation framework ADR.
///
/// - **Agents** — agent configurations keyed by role
/// - **Tools** — tool definitions available to agents
/// - **Leader** — role identifier of the lead agent
/// - **Pipeline** — ordered pipeline stage definitions
/// - **Governance** — governance policy identifier
/// - **Memory** — memory backend identifier
///
/// This is the resolved, operational form of `OrgConfig`.
#[derive(Debug, Clone)]
pub struct OrgStruct {
    /// Unique org identifier.
    pub id: String,

    /// Org name (e.g. "engineering", "operations").
    pub name: String,

    /// Agent configurations keyed by role.
    pub agents: HashMap<String, AgentConfig>,

    /// Tool definitions available to agents in this org.
    pub tools: Vec<ToolDefinition>,

    /// Role identifier of the lead agent for this org.
    pub leader: String,

    /// Ordered pipeline stage definitions.
    pub pipeline: Vec<PipelineStage>,

    /// Governance policy identifier (references a `ZephyrPolicy` by id).
    pub governance_policy_id: String,

    /// Memory backend identifier (e.g. "jsonl", "dolt").
    pub memory_backend_id: String,
}

impl OrgTrait for OrgStruct {}

impl OrgStruct {
    /// Construct a new `OrgStruct` with all six ADR properties.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        name: String,
        agents: HashMap<String, AgentConfig>,
        tools: Vec<ToolDefinition>,
        leader: String,
        pipeline: Vec<PipelineStage>,
        governance_policy_id: String,
        memory_backend_id: String,
    ) -> Self {
        Self {
            id,
            name,
            agents,
            tools,
            leader,
            pipeline,
            governance_policy_id,
            memory_backend_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Org trait — what an org can do
// ---------------------------------------------------------------------------

/// Defines the capabilities of an organisational unit.
///
/// An `Org` owns a configuration, can create budget trackers for tasks,
/// and exposes its identity. Concrete orgs implement this trait on top
/// of a loaded `OrgConfig`.
pub trait Org: Send + Sync {
    /// The org's human-readable name.
    fn name(&self) -> &str;

    /// Access the org's full configuration.
    fn config(&self) -> &OrgConfig;

    /// Create a new [`BudgetTracker`] sized to this org's per-task limits.
    fn budget_tracker(&self) -> BudgetTracker {
        Task::budget_tracker(&self.config().limits)
    }
}

// ---------------------------------------------------------------------------
// GlitchlabOrg — default Org backed by OrgConfig
// ---------------------------------------------------------------------------

/// Default org implementation for GLITCHLAB, backed by an [`OrgConfig`].
pub struct GlitchlabOrg {
    config: OrgConfig,
}

impl GlitchlabOrg {
    /// Create a new `GlitchlabOrg` from a loaded configuration.
    pub fn new(config: OrgConfig) -> Self {
        Self { config }
    }
}

impl Org for GlitchlabOrg {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn config(&self) -> &OrgConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governance::{BoundaryEnforcer, ZephyrPolicy};

    fn test_org_config() -> OrgConfig {
        OrgConfig {
            name: "test-org".into(),
            agents: std::collections::HashMap::new(),
            tools: ToolPolicy::new(vec!["cargo".into()], vec![]),
            pipeline: vec![],
            governance: ZephyrPolicy {
                boundaries: BoundaryEnforcer::new(vec![]),
                autonomy: 0.5,
            },
            routing: std::collections::HashMap::new(),
            limits: LimitsConfig::default(),
            intervention: InterventionConfig::default(),
        }
    }

    #[test]
    fn glitchlab_org_name() {
        let org = GlitchlabOrg::new(test_org_config());
        assert_eq!(org.name(), "test-org");
    }

    #[test]
    fn glitchlab_org_config_accessible() {
        let org = GlitchlabOrg::new(test_org_config());
        assert_eq!(org.config().limits.max_fix_attempts, 4);
        assert!(org.config().agents.is_empty());
    }

    #[test]
    fn glitchlab_org_budget_tracker() {
        let org = GlitchlabOrg::new(test_org_config());
        let tracker = org.budget_tracker();
        assert!(!tracker.exceeded());
        assert_eq!(tracker.max_tokens, 150_000);
        assert!((tracker.max_dollars - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn org_trait_default_budget_tracker_from_limits() {
        let mut config = test_org_config();
        config.limits.max_tokens_per_task = 50_000;
        config.limits.max_dollars_per_task = 5.0;
        let org = GlitchlabOrg::new(config);
        let tracker = org.budget_tracker();
        assert_eq!(tracker.max_tokens, 50_000);
        assert!((tracker.max_dollars - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn limits_config_default() {
        let limits = LimitsConfig::default();
        assert_eq!(limits.max_fix_attempts, 4);
        assert_eq!(limits.max_tokens_per_task, 150_000);
        assert!((limits.max_dollars_per_task - 10.0).abs() < f64::EPSILON);
        assert!(limits.require_plan_review);
        assert!(limits.require_pr_review);
        assert_eq!(limits.max_tool_turns, 20);
    }

    #[test]
    fn intervention_config_default() {
        let intervention = InterventionConfig::default();
        assert!(intervention.pause_after_plan);
        assert!(intervention.pause_before_pr);
        assert!(intervention.pause_on_core_change);
        assert!(intervention.pause_on_budget_exceeded);
    }

    #[test]
    fn task_budget_tracker_from_limits() {
        let limits = LimitsConfig::default();
        let tracker = Task::budget_tracker(&limits);
        assert!(!tracker.exceeded());
    }

    #[test]
    fn task_deserialization_defaults() {
        let json = r#"{"task_id": "test-1", "objective": "Do something"}"#;
        let task: Task = serde_json::from_str(json).unwrap();
        assert_eq!(task.task_id, "test-1");
        assert_eq!(task.risk_level, "low");
        assert_eq!(task.source, "local");
        assert!(task.constraints.is_empty());
    }

    #[test]
    fn agent_config_deserialization_defaults() {
        let json = r#"{"role": "planner", "persona": "Zap", "system_prompt": "You are Zap."}"#;
        let config: AgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.max_tokens, 4096);
        assert!((config.temperature - 0.2).abs() < f32::EPSILON);
        assert!(config.model.is_none());
    }

    #[test]
    fn limits_config_serde_roundtrip() {
        let limits = LimitsConfig::default();
        let json = serde_json::to_string(&limits).unwrap();
        let parsed: LimitsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.max_fix_attempts, limits.max_fix_attempts);
    }

    #[test]
    fn task_serde_roundtrip() {
        let task = Task {
            task_id: "t-1".into(),
            objective: "Fix it".into(),
            constraints: vec!["no deps".into()],
            acceptance_criteria: vec!["tests pass".into()],
            risk_level: "high".into(),
            source: "github".into(),
        };
        let json = serde_json::to_string(&task).unwrap();
        let parsed: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.task_id, "t-1");
        assert_eq!(parsed.source, "github");
    }

    // --- Org struct / trait tests ---

    #[test]
    fn org_new_sets_all_fields() {
        let org = OrgStruct::new(
            "org-1".into(),
            "Engineering".into(),
            HashMap::new(),
            vec![],
            "planner".into(),
            vec![],
            "zephyr-v1".into(),
            "jsonl".into(),
        );
        assert_eq!(org.id, "org-1");
        assert_eq!(org.name, "Engineering");
        assert!(org.agents.is_empty());
        assert!(org.tools.is_empty());
        assert_eq!(org.leader, "planner");
        assert!(org.pipeline.is_empty());
        assert_eq!(org.governance_policy_id, "zephyr-v1");
        assert_eq!(org.memory_backend_id, "jsonl");
    }

    #[test]
    fn org_implements_org_trait() {
        fn assert_is_org<T: OrgTrait>(_: &T) {}
        let org = OrgStruct::new(
            "org-2".into(),
            "Operations".into(),
            HashMap::new(),
            vec![],
            "ops-lead".into(),
            vec![],
            "zephyr-v1".into(),
            "dolt".into(),
        );
        assert_is_org(&org);
    }

    #[test]
    fn org_new_with_agents_tools_and_pipeline() {
        use crate::pipeline::PipelineStage;
        use crate::tool::ToolDefinition;

        let agent_cfg = AgentConfig {
            role: "planner".into(),
            persona: "Prof Zap".into(),
            system_prompt: "You are a planner.".into(),
            model: None,
            max_tokens: 4096,
            temperature: 0.2,
        };
        let tool = ToolDefinition {
            name: "read_file".into(),
            description: "Reads a file.".into(),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let stage = PipelineStage {
            name: "plan".into(),
            agent_role: "planner".into(),
            optional: false,
            condition: None,
        };
        let mut agents = HashMap::new();
        agents.insert("planner".to_string(), agent_cfg);

        let org = OrgStruct::new(
            "org-3".into(),
            "Engineering".into(),
            agents,
            vec![tool],
            "planner".into(),
            vec![stage],
            "zephyr-v1".into(),
            "jsonl".into(),
        );
        assert_eq!(org.agents.len(), 1);
        assert!(org.agents.contains_key("planner"));
        assert_eq!(org.tools.len(), 1);
        assert_eq!(org.tools[0].name, "read_file");
        assert_eq!(org.pipeline.len(), 1);
        assert_eq!(org.pipeline[0].name, "plan");
    }

    #[test]
    fn org_struct_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<OrgStruct>();
    }
}
