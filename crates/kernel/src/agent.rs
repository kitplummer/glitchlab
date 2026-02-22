use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error;

// ---------------------------------------------------------------------------
// Agent trait
// ---------------------------------------------------------------------------

/// A domain-specific LLM-powered worker.
///
/// Agents are stateless between invocations. All state lives in the
/// `AgentContext` passed to `execute` and the `AgentOutput` returned.
pub trait Agent: Send + Sync {
    /// Unique identifier within the org (e.g. "planner", "implementer").
    fn role(&self) -> &str;

    /// Human-readable persona name (e.g. "Professor Zap", "Patch").
    fn persona(&self) -> &str;

    /// Execute this agent's task.
    ///
    /// The agent builds a prompt from `ctx`, calls an LLM via the router,
    /// parses the structured response, and returns the result.
    fn execute(
        &self,
        ctx: &AgentContext,
    ) -> impl Future<Output = error::Result<AgentOutput>> + Send;
}

// ---------------------------------------------------------------------------
// AgentContext — everything an agent needs to do its work
// ---------------------------------------------------------------------------

/// Context passed to every agent invocation.
///
/// Mirrors the Python `AgentContext` but adds explicit budget awareness
/// and checkpoint data for Gastown-style recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContext {
    /// Unique task identifier.
    pub task_id: String,

    /// Full task description with all injected context
    /// (prelude, repo index, failure history, constraints).
    pub objective: String,

    /// Absolute path to the original repository.
    pub repo_path: String,

    /// Absolute path to the isolated worktree for this task.
    pub working_dir: String,

    /// Task-level constraints (from task definition + prelude).
    #[serde(default)]
    pub constraints: Vec<String>,

    /// Success criteria that must be met.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,

    /// Risk level: "low", "medium", "high".
    #[serde(default = "default_risk")]
    pub risk_level: String,

    /// Filename → content for files relevant to this agent.
    #[serde(default)]
    pub file_context: HashMap<String, String>,

    /// Output from the previous pipeline stage.
    #[serde(default)]
    pub previous_output: serde_json::Value,

    /// Agent-specific extra context (e.g. prelude_context, test_output).
    #[serde(default)]
    pub extra: HashMap<String, serde_json::Value>,
}

fn default_risk() -> String {
    "low".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_context_default_risk() {
        let json = r#"{"task_id": "t1", "objective": "test", "repo_path": "/tmp", "working_dir": "/tmp"}"#;
        let ctx: AgentContext = serde_json::from_str(json).unwrap();
        assert_eq!(ctx.risk_level, "low");
    }

    #[test]
    fn message_role_serde() {
        let role = MessageRole::System;
        let json = serde_json::to_string(&role).unwrap();
        assert_eq!(json, "\"system\"");

        let parsed: MessageRole = serde_json::from_str("\"assistant\"").unwrap();
        assert_eq!(parsed, MessageRole::Assistant);
    }

    #[test]
    fn agent_output_parse_error_default() {
        let json = r#"{"data": {}, "metadata": {"agent": "test", "model": "m", "tokens": 0, "cost": 0.0, "latency_ms": 0}}"#;
        let output: AgentOutput = serde_json::from_str(json).unwrap();
        assert!(!output.parse_error);
    }

    #[test]
    fn agent_context_defaults() {
        let json = r#"{"task_id": "t1", "objective": "test", "repo_path": "/tmp", "working_dir": "/tmp"}"#;
        let ctx: AgentContext = serde_json::from_str(json).unwrap();
        assert!(ctx.file_context.is_empty());
        assert!(ctx.constraints.is_empty());
        assert!(ctx.extra.is_empty());
        assert_eq!(ctx.previous_output, serde_json::Value::Null);
    }

    #[test]
    fn agent_context_serde_roundtrip() {
        let ctx = AgentContext {
            task_id: "t1".into(),
            objective: "Do stuff".into(),
            repo_path: "/tmp".into(),
            working_dir: "/tmp/wt".into(),
            constraints: vec!["no deps".into()],
            acceptance_criteria: vec!["tests pass".into()],
            risk_level: "medium".into(),
            file_context: HashMap::from([("src/lib.rs".into(), "code".into())]),
            previous_output: serde_json::json!({"plan": "do stuff"}),
            extra: HashMap::new(),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let parsed: AgentContext = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.task_id, "t1");
        assert_eq!(parsed.constraints.len(), 1);
    }
}

// ---------------------------------------------------------------------------
// AgentOutput — what an agent returns
// ---------------------------------------------------------------------------

/// Structured output from an agent invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutput {
    /// Parsed structured data from the LLM response.
    /// Schema varies per agent role.
    pub data: serde_json::Value,

    /// Metadata about the LLM call that produced this output.
    pub metadata: AgentMetadata,

    /// True if the output was constructed from a fallback/default
    /// because the LLM response couldn't be parsed.
    #[serde(default)]
    pub parse_error: bool,
}

/// Metadata tagged on every agent output for auditability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMetadata {
    /// Agent role that produced this output.
    pub agent: String,

    /// Resolved model string (e.g. "anthropic/claude-sonnet-4-20250514").
    pub model: String,

    /// Tokens consumed by this call.
    pub tokens: u64,

    /// Estimated cost in USD.
    pub cost: f64,

    /// Roundtrip latency in milliseconds.
    pub latency_ms: u64,
}

// ---------------------------------------------------------------------------
// Message types for LLM calls
// ---------------------------------------------------------------------------

/// A chat message for LLM completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
}

/// Standard chat roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
}
