//! TQM (Total Quality Management) post-run analyzer.
//!
//! Pure-function analyzer that scans an `OrchestratorResult` for common
//! anti-patterns and systemic issues. Returns a list of detected patterns
//! with severity, description, and threshold information.
//!
//! This module has no side effects — it does not create beads or write files.
//! The caller is responsible for logging or persisting the results.

use serde::{Deserialize, Serialize};

use crate::orchestrator::OrchestratorResult;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Thresholds for TQM pattern detection.
///
/// Defaults are tuned for the $100 budget run: aggressive enough to catch
/// real problems, lenient enough to avoid false positives on small runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TQMConfig {
    /// Number of times the same task can be decomposed before flagging.
    #[serde(default = "default_decomposition_loop")]
    pub decomposition_loop_threshold: u32,
    /// Number of deferred tasks before flagging scope creep.
    #[serde(default = "default_scope_creep")]
    pub scope_creep_threshold: u32,
    /// Consecutive failures from the same model class before flagging.
    #[serde(default = "default_model_degradation")]
    pub model_degradation_threshold: u32,
    /// Consecutive failures for the same task before flagging stuck agent.
    #[serde(default = "default_stuck_agents")]
    pub stuck_agents_threshold: u32,
    /// Number of test-failure runs before flagging flakiness.
    #[serde(default = "default_test_flakiness")]
    pub test_flakiness_threshold: u32,
    /// Fraction of architect rejections (0.0–1.0) before flagging.
    #[serde(default = "default_architect_rejection")]
    pub architect_rejection_rate_threshold: f64,
    /// Fraction of budget consumed (0.0–1.0) before flagging pressure.
    #[serde(default = "default_budget_pressure")]
    pub budget_pressure_threshold: f64,
    /// Number of setup/provider errors before flagging.
    #[serde(default = "default_provider_failures")]
    pub provider_failure_threshold: u32,

    // -- Remediation task generation --
    /// Whether to generate remediation tasks from detected patterns.
    #[serde(default)]
    pub remediation_enabled: bool,
    /// Priority for generated remediation tasks (lower = higher priority).
    #[serde(default = "default_remediation_priority")]
    pub remediation_priority: u32,
}

fn default_decomposition_loop() -> u32 {
    3
}
fn default_scope_creep() -> u32 {
    3
}
fn default_model_degradation() -> u32 {
    3
}
fn default_stuck_agents() -> u32 {
    3
}
fn default_test_flakiness() -> u32 {
    2
}
fn default_architect_rejection() -> f64 {
    0.5
}
fn default_budget_pressure() -> f64 {
    0.9
}
fn default_provider_failures() -> u32 {
    3
}
fn default_remediation_priority() -> u32 {
    5
}

impl Default for TQMConfig {
    fn default() -> Self {
        Self {
            decomposition_loop_threshold: default_decomposition_loop(),
            scope_creep_threshold: default_scope_creep(),
            model_degradation_threshold: default_model_degradation(),
            stuck_agents_threshold: default_stuck_agents(),
            test_flakiness_threshold: default_test_flakiness(),
            architect_rejection_rate_threshold: default_architect_rejection(),
            budget_pressure_threshold: default_budget_pressure(),
            provider_failure_threshold: default_provider_failures(),
            remediation_enabled: false,
            remediation_priority: default_remediation_priority(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pattern types
// ---------------------------------------------------------------------------

/// Kind of anti-pattern detected by the TQM analyzer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternKind {
    /// A task keeps getting decomposed without making progress.
    DecompositionLoop,
    /// Too many tasks are deferred, suggesting scope creep.
    ScopeCreep,
    /// Repeated failures suggest model quality degradation.
    ModelDegradation,
    /// The same task fails repeatedly without different approaches.
    StuckAgents,
    /// Tests fail intermittently across different tasks.
    TestFlakiness,
    /// High rate of architect review rejections.
    ArchitectRejectionRate,
    /// Budget consumption is approaching the limit.
    BudgetPressure,
    /// Provider/setup errors indicate infrastructure problems.
    ProviderFailures,
    /// A task hit a boundary/protected-path violation.
    BoundaryViolation,
}

/// A pattern detected by the TQM analyzer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedPattern {
    /// The kind of pattern detected.
    pub kind: PatternKind,
    /// Human-readable description of the issue.
    pub description: String,
    /// Number of occurrences that triggered the pattern.
    pub occurrences: u32,
    /// The threshold that was exceeded.
    pub threshold: f64,
    /// Severity level: "critical", "warning", or "info".
    pub severity: String,
    /// Task IDs affected by this pattern.
    pub affected_tasks: Vec<String>,
}

// ---------------------------------------------------------------------------
// Analyzer
// ---------------------------------------------------------------------------

/// Analyze an orchestrator result for common anti-patterns.
///
/// Returns a list of detected patterns. An empty list means no issues found.
pub fn analyze(result: &OrchestratorResult, config: &TQMConfig) -> Vec<DetectedPattern> {
    let mut patterns = Vec::new();

    check_decomposition_loops(result, config, &mut patterns);
    check_scope_creep(result, config, &mut patterns);
    check_model_degradation(result, config, &mut patterns);
    check_stuck_agents(result, config, &mut patterns);
    check_test_flakiness(result, config, &mut patterns);
    check_architect_rejection_rate(result, config, &mut patterns);
    check_budget_pressure(result, config, &mut patterns);
    check_provider_failures(result, config, &mut patterns);
    check_boundary_violations(result, config, &mut patterns);

    patterns
}

/// Incremental variant of [`analyze`] for use during a live orchestrator run.
///
/// Runs the same detectors but filters out patterns whose `kind` already
/// appears in `previous_patterns`, so that each pattern fires at most once
/// per orchestrator run.
///
/// Also applies a minimum-sample guard to the architect rejection rate
/// detector: it is skipped when `result.tasks_attempted < 5` to avoid
/// false positives early in the run.
pub fn analyze_incremental(
    result: &OrchestratorResult,
    config: &TQMConfig,
    previous_patterns: &[DetectedPattern],
) -> Vec<DetectedPattern> {
    let already_detected: std::collections::HashSet<PatternKind> =
        previous_patterns.iter().map(|p| p.kind).collect();

    let mut patterns = Vec::new();

    check_decomposition_loops(result, config, &mut patterns);
    check_scope_creep(result, config, &mut patterns);
    check_model_degradation(result, config, &mut patterns);
    check_stuck_agents(result, config, &mut patterns);
    check_test_flakiness(result, config, &mut patterns);

    // Minimum sample guard: skip architect rejection rate when too few tasks attempted.
    if result.tasks_attempted >= 5 {
        check_architect_rejection_rate(result, config, &mut patterns);
    }

    check_budget_pressure(result, config, &mut patterns);
    check_provider_failures(result, config, &mut patterns);
    check_boundary_violations(result, config, &mut patterns);

    // Filter out patterns whose kind was already detected in a previous call.
    patterns.retain(|p| !already_detected.contains(&p.kind));

    patterns
}

/// Check for tasks that were decomposed multiple times (decomposition loops).
fn check_decomposition_loops(
    result: &OrchestratorResult,
    config: &TQMConfig,
    patterns: &mut Vec<DetectedPattern>,
) {
    let mut decomposition_counts: std::collections::HashMap<&str, u32> =
        std::collections::HashMap::new();
    for run in &result.run_results {
        if run.status.contains("Decomposed") || run.status.contains("decomposed") {
            *decomposition_counts.entry(&run.task_id).or_insert(0) += 1;
        }
    }

    let affected: Vec<String> = decomposition_counts
        .iter()
        .filter(|(_, count)| **count >= config.decomposition_loop_threshold)
        .map(|(id, _)| id.to_string())
        .collect();

    if !affected.is_empty() {
        let total: u32 = decomposition_counts
            .values()
            .filter(|c| **c >= config.decomposition_loop_threshold)
            .sum();
        patterns.push(DetectedPattern {
            kind: PatternKind::DecompositionLoop,
            description: format!(
                "{} task(s) decomposed {} or more times without progress",
                affected.len(),
                config.decomposition_loop_threshold
            ),
            occurrences: total,
            threshold: config.decomposition_loop_threshold as f64,
            severity: "critical".into(),
            affected_tasks: affected,
        });
    }
}

/// Check for scope creep (too many deferred tasks).
fn check_scope_creep(
    result: &OrchestratorResult,
    config: &TQMConfig,
    patterns: &mut Vec<DetectedPattern>,
) {
    if result.tasks_deferred >= config.scope_creep_threshold {
        let affected: Vec<String> = result
            .run_results
            .iter()
            .filter(|r| {
                r.status.contains("Deferred")
                    || r.status.contains("deferred")
                    || r.status.contains("Retryable")
                    || r.status.contains("retryable")
            })
            .map(|r| r.task_id.clone())
            .collect();
        patterns.push(DetectedPattern {
            kind: PatternKind::ScopeCreep,
            description: format!(
                "{} tasks deferred (threshold: {})",
                result.tasks_deferred, config.scope_creep_threshold
            ),
            occurrences: result.tasks_deferred,
            threshold: config.scope_creep_threshold as f64,
            severity: "warning".into(),
            affected_tasks: affected,
        });
    }
}

/// Check for model degradation (repeated model-related failures).
fn check_model_degradation(
    result: &OrchestratorResult,
    config: &TQMConfig,
    patterns: &mut Vec<DetectedPattern>,
) {
    use glitchlab_kernel::outcome::ObstacleKind;

    let mut model_failure_count = 0u32;
    let mut affected = Vec::new();
    for run in &result.run_results {
        if let Some(ref ctx) = run.outcome_context
            && matches!(
                ctx.obstacle,
                ObstacleKind::ModelLimitation { .. } | ObstacleKind::ParseFailure { .. }
            )
        {
            model_failure_count += 1;
            affected.push(run.task_id.clone());
        }
    }

    if model_failure_count >= config.model_degradation_threshold {
        patterns.push(DetectedPattern {
            kind: PatternKind::ModelDegradation,
            description: format!(
                "{model_failure_count} model-related failures (limitation/parse) detected"
            ),
            occurrences: model_failure_count,
            threshold: config.model_degradation_threshold as f64,
            severity: "warning".into(),
            affected_tasks: affected,
        });
    }
}

/// Check for stuck agents (same task failing repeatedly).
fn check_stuck_agents(
    result: &OrchestratorResult,
    config: &TQMConfig,
    patterns: &mut Vec<DetectedPattern>,
) {
    let mut failure_counts: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for run in &result.run_results {
        // Count non-success statuses (not PrCreated, Committed, PrMerged, AlreadyDone, Decomposed)
        let is_failure = !matches!(
            run.status.as_str(),
            "PrCreated" | "Committed" | "PrMerged" | "AlreadyDone" | "Decomposed"
        );
        if is_failure {
            *failure_counts.entry(&run.task_id).or_insert(0) += 1;
        }
    }

    let affected: Vec<String> = failure_counts
        .iter()
        .filter(|(_, count)| **count >= config.stuck_agents_threshold)
        .map(|(id, _)| id.to_string())
        .collect();

    if !affected.is_empty() {
        let total: u32 = failure_counts
            .values()
            .filter(|c| **c >= config.stuck_agents_threshold)
            .sum();
        patterns.push(DetectedPattern {
            kind: PatternKind::StuckAgents,
            description: format!(
                "{} task(s) failed {} or more times",
                affected.len(),
                config.stuck_agents_threshold
            ),
            occurrences: total,
            threshold: config.stuck_agents_threshold as f64,
            severity: "warning".into(),
            affected_tasks: affected,
        });
    }
}

/// Check for test flakiness (test failures across different tasks).
fn check_test_flakiness(
    result: &OrchestratorResult,
    config: &TQMConfig,
    patterns: &mut Vec<DetectedPattern>,
) {
    let mut affected = Vec::new();
    for run in &result.run_results {
        if run.status.contains("TestsFailed") || run.status.contains("tests_failed") {
            affected.push(run.task_id.clone());
        }
    }

    let count = affected.len() as u32;
    if count >= config.test_flakiness_threshold {
        patterns.push(DetectedPattern {
            kind: PatternKind::TestFlakiness,
            description: format!("{count} task runs ended with test failures"),
            occurrences: count,
            threshold: config.test_flakiness_threshold as f64,
            severity: "warning".into(),
            affected_tasks: affected,
        });
    }
}

/// Check for high architect rejection rate.
fn check_architect_rejection_rate(
    result: &OrchestratorResult,
    config: &TQMConfig,
    patterns: &mut Vec<DetectedPattern>,
) {
    if result.tasks_attempted == 0 {
        return;
    }

    let mut rejection_count = 0u32;
    let mut affected = Vec::new();
    for run in &result.run_results {
        if run.status.contains("ArchitectRejected") || run.status.contains("architect_rejected") {
            rejection_count += 1;
            affected.push(run.task_id.clone());
        }
    }

    let rate = rejection_count as f64 / result.tasks_attempted as f64;
    if rate >= config.architect_rejection_rate_threshold && rejection_count > 0 {
        patterns.push(DetectedPattern {
            kind: PatternKind::ArchitectRejectionRate,
            description: format!(
                "Architect rejection rate {:.0}% ({rejection_count}/{}) exceeds threshold {:.0}%",
                rate * 100.0,
                result.tasks_attempted,
                config.architect_rejection_rate_threshold * 100.0
            ),
            occurrences: rejection_count,
            threshold: config.architect_rejection_rate_threshold,
            severity: "warning".into(),
            affected_tasks: affected,
        });
    }
}

/// Check for budget pressure.
fn check_budget_pressure(
    result: &OrchestratorResult,
    config: &TQMConfig,
    patterns: &mut Vec<DetectedPattern>,
) {
    // We use cease_reason as a proxy — if budget was exhausted, it's clearly pressure.
    // For partial runs, we'd need the budget ceiling, which isn't in OrchestratorResult.
    // Flag if budget exhaustion was the cease reason.
    if result.cease_reason == crate::orchestrator::CeaseReason::BudgetExhausted {
        patterns.push(DetectedPattern {
            kind: PatternKind::BudgetPressure,
            description: format!(
                "Budget exhausted after {} tasks (${:.2} spent)",
                result.tasks_attempted, result.total_cost
            ),
            occurrences: 1,
            threshold: config.budget_pressure_threshold,
            severity: "info".into(),
            affected_tasks: vec![],
        });
    }
}

/// Check for provider/infrastructure failures.
fn check_provider_failures(
    result: &OrchestratorResult,
    config: &TQMConfig,
    patterns: &mut Vec<DetectedPattern>,
) {
    let mut affected = Vec::new();
    for run in &result.run_results {
        if run.status == "setup_error" {
            affected.push(run.task_id.clone());
        } else if let Some(ref ctx) = run.outcome_context
            && matches!(
                ctx.obstacle,
                glitchlab_kernel::outcome::ObstacleKind::ExternalDependency { .. }
            )
        {
            affected.push(run.task_id.clone());
        }
    }

    let count = affected.len() as u32;
    if count >= config.provider_failure_threshold {
        patterns.push(DetectedPattern {
            kind: PatternKind::ProviderFailures,
            description: format!(
                "{count} provider/infrastructure failures (setup errors or external dependencies)"
            ),
            occurrences: count,
            threshold: config.provider_failure_threshold as f64,
            severity: "critical".into(),
            affected_tasks: affected,
        });
    }
}

/// Check for boundary/protected-path violations.
///
/// Fires on the first occurrence — no threshold configuration needed.
fn check_boundary_violations(
    result: &OrchestratorResult,
    _config: &TQMConfig,
    patterns: &mut Vec<DetectedPattern>,
) {
    let mut affected = Vec::new();
    for run in &result.run_results {
        let error_text = run.error.as_deref().unwrap_or("");
        if run.status == "BoundaryViolation"
            || (run.status == "ImplementationFailed"
                && (error_text.contains("PROTECTED PATH")
                    || error_text.contains("boundary_violation")))
        {
            affected.push(run.task_id.clone());
        }
    }
    if !affected.is_empty() {
        let count = affected.len() as u32;
        patterns.push(DetectedPattern {
            kind: PatternKind::BoundaryViolation,
            description: format!("{count} task(s) hit boundary violations (protected path)"),
            occurrences: count,
            threshold: 1.0,
            severity: "warning".into(),
            affected_tasks: affected,
        });
    }
}

// ---------------------------------------------------------------------------
// Remediation task generation
// ---------------------------------------------------------------------------

/// Map a `PatternKind` to a short slug for use in remediation task IDs.
fn kind_slug(kind: PatternKind) -> &'static str {
    match kind {
        PatternKind::DecompositionLoop => "decomp-loop",
        PatternKind::ScopeCreep => "scope-creep",
        PatternKind::ModelDegradation => "model-degrad",
        PatternKind::StuckAgents => "stuck-agents",
        PatternKind::TestFlakiness => "test-flaky",
        PatternKind::ArchitectRejectionRate => "arch-reject",
        PatternKind::BudgetPressure => "budget",
        PatternKind::ProviderFailures => "provider",
        PatternKind::BoundaryViolation => "boundary",
    }
}

/// Whether a pattern kind produces an actionable remediation task.
fn is_actionable(kind: PatternKind) -> bool {
    !matches!(
        kind,
        PatternKind::BudgetPressure | PatternKind::ProviderFailures
    )
}

/// Generate a deterministic remediation task ID from the pattern kind and affected task IDs.
fn remediation_task_id(kind: PatternKind, affected: &[String]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut sorted = affected.to_vec();
    sorted.sort();

    let mut hasher = DefaultHasher::new();
    for id in &sorted {
        id.hash(&mut hasher);
    }
    let hash = hasher.finish();
    format!("tqm-{}-{:016x}", kind_slug(kind), hash)
}

/// Convert a single detected pattern into a remediation task (if actionable).
fn pattern_to_task(pattern: &DetectedPattern, priority: u32) -> Option<crate::taskqueue::Task> {
    if !is_actionable(pattern.kind) {
        return None;
    }

    let objective = match pattern.kind {
        PatternKind::DecompositionLoop => {
            let ids = pattern.affected_tasks.join(", ");
            format!("TQM: Simplify tasks stuck in decomposition loops: {ids}")
        }
        PatternKind::ScopeCreep => {
            format!(
                "TQM: Review and reprioritize {} deferred tasks",
                pattern.occurrences
            )
        }
        PatternKind::ModelDegradation => {
            format!(
                "TQM: Investigate {} model-related failures",
                pattern.occurrences
            )
        }
        PatternKind::StuckAgents => {
            let ids = pattern.affected_tasks.join(", ");
            format!("TQM: Investigate stuck tasks {ids}")
        }
        PatternKind::TestFlakiness => {
            format!(
                "TQM: Fix flaky tests affecting {} task runs",
                pattern.occurrences
            )
        }
        PatternKind::ArchitectRejectionRate => "TQM: Review architect rejection patterns".into(),
        PatternKind::BoundaryViolation => {
            let ids = pattern.affected_tasks.join(", ");
            format!("TQM: Investigate boundary violations for tasks: {ids}")
        }
        // Advisory-only patterns — not actionable.
        PatternKind::BudgetPressure | PatternKind::ProviderFailures => return None,
    };

    let id = remediation_task_id(pattern.kind, &pattern.affected_tasks);

    Some(crate::taskqueue::Task {
        id,
        objective,
        priority,
        status: crate::taskqueue::TaskStatus::Pending,
        depends_on: vec![],
        decomposition_depth: 0,
        error: None,
        pr_url: None,
        outcome_context: None,
    })
}

/// Generate remediation tasks from detected patterns.
///
/// Returns one task per actionable pattern. Advisory-only patterns
/// (`BudgetPressure`, `ProviderFailures`) are skipped.
pub fn generate_remediation_tasks(
    patterns: &[DetectedPattern],
    config: &TQMConfig,
) -> Vec<crate::taskqueue::Task> {
    patterns
        .iter()
        .filter_map(|p| pattern_to_task(p, config.remediation_priority))
        .collect()
}

/// Generate remediation tasks, filtering out duplicates already in the queue.
///
/// Returns an empty vec if `config.remediation_enabled` is false.
/// Skips patterns whose kind already has a pending/in-progress remediation task.
pub fn generate_deduplicated_remediation_tasks(
    patterns: &[DetectedPattern],
    config: &TQMConfig,
    queue: &crate::taskqueue::TaskQueue,
) -> Vec<crate::taskqueue::Task> {
    if !config.remediation_enabled {
        return Vec::new();
    }

    let filtered: Vec<&DetectedPattern> = patterns
        .iter()
        .filter(|p| {
            is_actionable(p.kind)
                && !queue.has_pending_with_prefix(&format!("tqm-{}", kind_slug(p.kind)))
        })
        .collect();

    filtered
        .iter()
        .filter_map(|p| pattern_to_task(p, config.remediation_priority))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::{CeaseReason, OrchestratorResult, TaskRunResult};
    use glitchlab_kernel::outcome::{ObstacleKind, OutcomeContext};
    use std::time::Duration;

    fn empty_result() -> OrchestratorResult {
        OrchestratorResult {
            tasks_attempted: 0,
            tasks_succeeded: 0,
            tasks_failed: 0,
            tasks_deferred: 0,
            tasks_escalated: 0,
            total_cost: 0.0,
            total_tokens: 0,
            duration: Duration::from_secs(0),
            cease_reason: CeaseReason::AllTasksDone,
            run_results: Vec::new(),
            dominant_failure_pattern: None,
        }
    }

    fn run_result(task_id: &str, status: &str) -> TaskRunResult {
        TaskRunResult {
            task_id: task_id.into(),
            status: status.into(),
            cost: 0.5,
            tokens: 1000,
            pr_url: None,
            error: None,
            outcome_context: None,
        }
    }

    fn run_result_with_context(
        task_id: &str,
        status: &str,
        obstacle: ObstacleKind,
    ) -> TaskRunResult {
        TaskRunResult {
            task_id: task_id.into(),
            status: status.into(),
            cost: 0.5,
            tokens: 1000,
            pr_url: None,
            error: None,
            outcome_context: Some(OutcomeContext {
                approach: "tried something".into(),
                obstacle,
                discoveries: vec![],
                recommendation: None,
                files_explored: vec![],
            }),
        }
    }

    #[test]
    fn analyze_empty_result() {
        let result = empty_result();
        let patterns = analyze(&result, &TQMConfig::default());
        assert!(patterns.is_empty());
    }

    #[test]
    fn decomposition_loop_below_threshold() {
        let mut result = empty_result();
        result.tasks_attempted = 2;
        // 2 decompositions for same task (threshold is 3)
        result.run_results = vec![
            run_result("t1", "Decomposed"),
            run_result("t1", "Decomposed"),
        ];
        let patterns = analyze(&result, &TQMConfig::default());
        assert!(
            !patterns
                .iter()
                .any(|p| p.kind == PatternKind::DecompositionLoop)
        );
    }

    #[test]
    fn decomposition_loop_at_threshold() {
        let mut result = empty_result();
        result.tasks_attempted = 3;
        result.run_results = vec![
            run_result("t1", "Decomposed"),
            run_result("t1", "Decomposed"),
            run_result("t1", "Decomposed"),
        ];
        let patterns = analyze(&result, &TQMConfig::default());
        let decomp = patterns
            .iter()
            .find(|p| p.kind == PatternKind::DecompositionLoop);
        assert!(decomp.is_some());
        let d = decomp.unwrap();
        assert_eq!(d.severity, "critical");
        assert_eq!(d.affected_tasks, vec!["t1"]);
    }

    #[test]
    fn scope_creep_below_threshold() {
        let mut result = empty_result();
        result.tasks_deferred = 2; // threshold is 3
        let patterns = analyze(&result, &TQMConfig::default());
        assert!(!patterns.iter().any(|p| p.kind == PatternKind::ScopeCreep));
    }

    #[test]
    fn scope_creep_at_threshold() {
        let mut result = empty_result();
        result.tasks_attempted = 5;
        result.tasks_deferred = 3;
        result.run_results = vec![
            run_result("t1", "Deferred"),
            run_result("t2", "Deferred"),
            run_result("t3", "Deferred"),
        ];
        let patterns = analyze(&result, &TQMConfig::default());
        let scope = patterns.iter().find(|p| p.kind == PatternKind::ScopeCreep);
        assert!(scope.is_some());
        assert_eq!(scope.unwrap().severity, "warning");
    }

    #[test]
    fn model_degradation_detected() {
        let mut result = empty_result();
        result.tasks_attempted = 3;
        result.run_results = vec![
            run_result_with_context(
                "t1",
                "Error",
                ObstacleKind::ModelLimitation {
                    model: "gpt-4".into(),
                    error_class: "context_overflow".into(),
                },
            ),
            run_result_with_context(
                "t2",
                "Error",
                ObstacleKind::ParseFailure {
                    model: "gpt-4".into(),
                    raw_snippet: "{bad".into(),
                },
            ),
            run_result_with_context(
                "t3",
                "Error",
                ObstacleKind::ModelLimitation {
                    model: "gpt-4".into(),
                    error_class: "timeout".into(),
                },
            ),
        ];
        let patterns = analyze(&result, &TQMConfig::default());
        let model = patterns
            .iter()
            .find(|p| p.kind == PatternKind::ModelDegradation);
        assert!(model.is_some());
        assert_eq!(model.unwrap().occurrences, 3);
    }

    #[test]
    fn stuck_agents_detected() {
        let mut result = empty_result();
        result.tasks_attempted = 3;
        result.run_results = vec![
            run_result("t1", "Error"),
            run_result("t1", "Error"),
            run_result("t1", "Error"),
        ];
        let patterns = analyze(&result, &TQMConfig::default());
        let stuck = patterns.iter().find(|p| p.kind == PatternKind::StuckAgents);
        assert!(stuck.is_some());
        assert_eq!(stuck.unwrap().affected_tasks, vec!["t1"]);
    }

    #[test]
    fn test_flakiness_below_threshold() {
        let mut result = empty_result();
        result.tasks_attempted = 1;
        result.run_results = vec![run_result("t1", "TestsFailed")];
        let patterns = analyze(&result, &TQMConfig::default());
        assert!(
            !patterns
                .iter()
                .any(|p| p.kind == PatternKind::TestFlakiness)
        );
    }

    #[test]
    fn test_flakiness_at_threshold() {
        let mut result = empty_result();
        result.tasks_attempted = 2;
        result.run_results = vec![
            run_result("t1", "TestsFailed"),
            run_result("t2", "TestsFailed"),
        ];
        let patterns = analyze(&result, &TQMConfig::default());
        let flaky = patterns
            .iter()
            .find(|p| p.kind == PatternKind::TestFlakiness);
        assert!(flaky.is_some());
        assert_eq!(flaky.unwrap().occurrences, 2);
    }

    #[test]
    fn architect_rejection_rate_below_threshold() {
        let mut result = empty_result();
        result.tasks_attempted = 10;
        result.run_results = vec![
            run_result("t1", "ArchitectRejected"),
            run_result("t2", "PrCreated"),
            run_result("t3", "PrCreated"),
            run_result("t4", "PrCreated"),
        ];
        // 1/10 = 10% < 50%
        let patterns = analyze(&result, &TQMConfig::default());
        assert!(
            !patterns
                .iter()
                .any(|p| p.kind == PatternKind::ArchitectRejectionRate)
        );
    }

    #[test]
    fn architect_rejection_rate_at_threshold() {
        let mut result = empty_result();
        result.tasks_attempted = 4;
        result.run_results = vec![
            run_result("t1", "ArchitectRejected"),
            run_result("t2", "ArchitectRejected"),
            run_result("t3", "PrCreated"),
            run_result("t4", "PrCreated"),
        ];
        // 2/4 = 50% >= 50%
        let patterns = analyze(&result, &TQMConfig::default());
        let rejection = patterns
            .iter()
            .find(|p| p.kind == PatternKind::ArchitectRejectionRate);
        assert!(rejection.is_some());
    }

    #[test]
    fn budget_pressure_when_exhausted() {
        let mut result = empty_result();
        result.tasks_attempted = 5;
        result.total_cost = 95.0;
        result.cease_reason = CeaseReason::BudgetExhausted;
        let patterns = analyze(&result, &TQMConfig::default());
        let budget = patterns
            .iter()
            .find(|p| p.kind == PatternKind::BudgetPressure);
        assert!(budget.is_some());
        assert_eq!(budget.unwrap().severity, "info");
    }

    #[test]
    fn budget_pressure_not_when_complete() {
        let mut result = empty_result();
        result.tasks_attempted = 5;
        result.total_cost = 95.0;
        result.cease_reason = CeaseReason::AllTasksDone;
        let patterns = analyze(&result, &TQMConfig::default());
        assert!(
            !patterns
                .iter()
                .any(|p| p.kind == PatternKind::BudgetPressure)
        );
    }

    #[test]
    fn provider_failures_detected() {
        let mut result = empty_result();
        result.tasks_attempted = 3;
        result.run_results = vec![
            run_result("t1", "setup_error"),
            run_result("t2", "setup_error"),
            run_result_with_context(
                "t3",
                "Error",
                ObstacleKind::ExternalDependency {
                    service: "npm".into(),
                },
            ),
        ];
        let patterns = analyze(&result, &TQMConfig::default());
        let provider = patterns
            .iter()
            .find(|p| p.kind == PatternKind::ProviderFailures);
        assert!(provider.is_some());
        assert_eq!(provider.unwrap().severity, "critical");
        assert_eq!(provider.unwrap().occurrences, 3);
    }

    #[test]
    fn provider_failures_below_threshold() {
        let mut result = empty_result();
        result.tasks_attempted = 2;
        result.run_results = vec![
            run_result("t1", "setup_error"),
            run_result("t2", "setup_error"),
        ];
        let patterns = analyze(&result, &TQMConfig::default());
        assert!(
            !patterns
                .iter()
                .any(|p| p.kind == PatternKind::ProviderFailures)
        );
    }

    #[test]
    fn multiple_simultaneous_patterns() {
        let mut result = empty_result();
        result.tasks_attempted = 6;
        result.tasks_deferred = 3;
        result.cease_reason = CeaseReason::BudgetExhausted;
        result.total_cost = 95.0;
        result.run_results = vec![
            run_result("t1", "Deferred"),
            run_result("t2", "Deferred"),
            run_result("t3", "Deferred"),
            run_result("t4", "setup_error"),
            run_result("t5", "setup_error"),
            run_result("t6", "setup_error"),
        ];
        let patterns = analyze(&result, &TQMConfig::default());
        let kinds: Vec<PatternKind> = patterns.iter().map(|p| p.kind).collect();
        assert!(kinds.contains(&PatternKind::ScopeCreep));
        assert!(kinds.contains(&PatternKind::BudgetPressure));
        assert!(kinds.contains(&PatternKind::ProviderFailures));
    }

    #[test]
    fn config_defaults() {
        let config = TQMConfig::default();
        assert_eq!(config.decomposition_loop_threshold, 3);
        assert_eq!(config.scope_creep_threshold, 3);
        assert_eq!(config.model_degradation_threshold, 3);
        assert_eq!(config.stuck_agents_threshold, 3);
        assert_eq!(config.test_flakiness_threshold, 2);
        assert!((config.architect_rejection_rate_threshold - 0.5).abs() < f64::EPSILON);
        assert!((config.budget_pressure_threshold - 0.9).abs() < f64::EPSILON);
        assert_eq!(config.provider_failure_threshold, 3);
        assert!(!config.remediation_enabled);
        assert_eq!(config.remediation_priority, 5);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = TQMConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: TQMConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.decomposition_loop_threshold,
            config.decomposition_loop_threshold
        );
        assert_eq!(parsed.scope_creep_threshold, config.scope_creep_threshold);
    }

    #[test]
    fn pattern_kind_serde_roundtrip() {
        for kind in [
            PatternKind::DecompositionLoop,
            PatternKind::ScopeCreep,
            PatternKind::ModelDegradation,
            PatternKind::StuckAgents,
            PatternKind::TestFlakiness,
            PatternKind::ArchitectRejectionRate,
            PatternKind::BudgetPressure,
            PatternKind::ProviderFailures,
            PatternKind::BoundaryViolation,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let parsed: PatternKind = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, kind);
        }
    }

    #[test]
    fn detected_pattern_serde_roundtrip() {
        let pattern = DetectedPattern {
            kind: PatternKind::TestFlakiness,
            description: "test description".into(),
            occurrences: 5,
            threshold: 2.0,
            severity: "warning".into(),
            affected_tasks: vec!["t1".into(), "t2".into()],
        };
        let json = serde_json::to_string(&pattern).unwrap();
        let parsed: DetectedPattern = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.kind, PatternKind::TestFlakiness);
        assert_eq!(parsed.occurrences, 5);
        assert_eq!(parsed.affected_tasks.len(), 2);
    }

    #[test]
    fn config_serde_backward_compat_without_remediation() {
        // Configs serialised before remediation fields existed should still parse.
        let json = r#"{
            "decomposition_loop_threshold": 3,
            "scope_creep_threshold": 3,
            "model_degradation_threshold": 3,
            "stuck_agents_threshold": 3,
            "test_flakiness_threshold": 2,
            "architect_rejection_rate_threshold": 0.5,
            "budget_pressure_threshold": 0.9,
            "provider_failure_threshold": 3
        }"#;
        let parsed: TQMConfig = serde_json::from_str(json).unwrap();
        assert!(!parsed.remediation_enabled);
        assert_eq!(parsed.remediation_priority, 5);
    }

    // -----------------------------------------------------------------------
    // Remediation task generation tests
    // -----------------------------------------------------------------------

    fn make_pattern(kind: PatternKind, affected: Vec<String>) -> DetectedPattern {
        DetectedPattern {
            kind,
            description: format!("{kind:?} detected"),
            occurrences: affected.len() as u32,
            threshold: 3.0,
            severity: "warning".into(),
            affected_tasks: affected,
        }
    }

    #[test]
    fn remediation_empty_patterns() {
        let config = TQMConfig::default();
        let tasks = generate_remediation_tasks(&[], &config);
        assert!(tasks.is_empty());
    }

    #[test]
    fn remediation_advisory_kinds_skipped() {
        let config = TQMConfig::default();
        let patterns = vec![
            make_pattern(PatternKind::BudgetPressure, vec!["t1".into()]),
            make_pattern(PatternKind::ProviderFailures, vec!["t2".into()]),
        ];
        let tasks = generate_remediation_tasks(&patterns, &config);
        assert!(tasks.is_empty());
    }

    #[test]
    fn remediation_decomposition_loop_produces_task() {
        let config = TQMConfig::default();
        let patterns = vec![make_pattern(
            PatternKind::DecompositionLoop,
            vec!["t1".into(), "t2".into()],
        )];
        let tasks = generate_remediation_tasks(&patterns, &config);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].id.starts_with("tqm-decomp-loop-"));
        assert!(tasks[0].objective.contains("decomposition loops"));
        assert!(tasks[0].objective.contains("t1"));
        assert_eq!(tasks[0].priority, 5);
    }

    #[test]
    fn remediation_scope_creep_produces_task() {
        let config = TQMConfig::default();
        let patterns = vec![make_pattern(
            PatternKind::ScopeCreep,
            vec!["t1".into(), "t2".into(), "t3".into()],
        )];
        let tasks = generate_remediation_tasks(&patterns, &config);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].id.starts_with("tqm-scope-creep-"));
        assert!(tasks[0].objective.contains("deferred tasks"));
    }

    #[test]
    fn remediation_model_degradation_produces_task() {
        let config = TQMConfig::default();
        let patterns = vec![make_pattern(
            PatternKind::ModelDegradation,
            vec!["t1".into()],
        )];
        let tasks = generate_remediation_tasks(&patterns, &config);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].id.starts_with("tqm-model-degrad-"));
        assert!(tasks[0].objective.contains("model-related failures"));
    }

    #[test]
    fn remediation_stuck_agents_produces_task() {
        let config = TQMConfig::default();
        let patterns = vec![make_pattern(PatternKind::StuckAgents, vec!["t1".into()])];
        let tasks = generate_remediation_tasks(&patterns, &config);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].id.starts_with("tqm-stuck-agents-"));
        assert!(tasks[0].objective.contains("stuck tasks"));
    }

    #[test]
    fn remediation_test_flakiness_produces_task() {
        let config = TQMConfig::default();
        let patterns = vec![make_pattern(
            PatternKind::TestFlakiness,
            vec!["t1".into(), "t2".into()],
        )];
        let tasks = generate_remediation_tasks(&patterns, &config);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].id.starts_with("tqm-test-flaky-"));
        assert!(tasks[0].objective.contains("flaky tests"));
    }

    #[test]
    fn remediation_architect_rejection_produces_task() {
        let config = TQMConfig::default();
        let patterns = vec![make_pattern(
            PatternKind::ArchitectRejectionRate,
            vec!["t1".into()],
        )];
        let tasks = generate_remediation_tasks(&patterns, &config);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].id.starts_with("tqm-arch-reject-"));
        assert!(tasks[0].objective.contains("architect rejection"));
    }

    #[test]
    fn remediation_deterministic_ids() {
        let config = TQMConfig::default();
        let patterns = vec![make_pattern(
            PatternKind::StuckAgents,
            vec!["t2".into(), "t1".into()],
        )];
        let tasks1 = generate_remediation_tasks(&patterns, &config);
        let tasks2 = generate_remediation_tasks(&patterns, &config);
        assert_eq!(tasks1[0].id, tasks2[0].id);
    }

    #[test]
    fn remediation_different_affected_different_ids() {
        let config = TQMConfig::default();
        let p1 = vec![make_pattern(PatternKind::StuckAgents, vec!["t1".into()])];
        let p2 = vec![make_pattern(PatternKind::StuckAgents, vec!["t2".into()])];
        let t1 = generate_remediation_tasks(&p1, &config);
        let t2 = generate_remediation_tasks(&p2, &config);
        assert_ne!(t1[0].id, t2[0].id);
    }

    #[test]
    fn remediation_respects_priority_config() {
        let config = TQMConfig {
            remediation_priority: 42,
            ..TQMConfig::default()
        };
        let patterns = vec![make_pattern(PatternKind::TestFlakiness, vec!["t1".into()])];
        let tasks = generate_remediation_tasks(&patterns, &config);
        assert_eq!(tasks[0].priority, 42);
    }

    #[test]
    fn remediation_multiple_patterns() {
        let config = TQMConfig::default();
        let patterns = vec![
            make_pattern(PatternKind::StuckAgents, vec!["t1".into()]),
            make_pattern(PatternKind::TestFlakiness, vec!["t2".into()]),
            make_pattern(PatternKind::BudgetPressure, vec![]),
        ];
        let tasks = generate_remediation_tasks(&patterns, &config);
        // BudgetPressure is advisory, so only 2 tasks.
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn dedup_returns_empty_when_disabled() {
        let config = TQMConfig::default(); // remediation_enabled = false
        let queue = crate::taskqueue::TaskQueue::from_tasks(vec![]);
        let patterns = vec![make_pattern(PatternKind::StuckAgents, vec!["t1".into()])];
        let tasks = generate_deduplicated_remediation_tasks(&patterns, &config, &queue);
        assert!(tasks.is_empty());
    }

    #[test]
    fn dedup_skips_existing_remediation_tasks() {
        let config = TQMConfig {
            remediation_enabled: true,
            ..TQMConfig::default()
        };

        // Put an existing tqm-stuck-agents task in the queue.
        let existing = crate::taskqueue::Task {
            id: "tqm-stuck-agents-aaaa".into(),
            objective: "existing".into(),
            priority: 5,
            status: crate::taskqueue::TaskStatus::Pending,
            depends_on: vec![],
            decomposition_depth: 0,
            error: None,
            pr_url: None,
            outcome_context: None,
        };
        let queue = crate::taskqueue::TaskQueue::from_tasks(vec![existing]);

        let patterns = vec![
            make_pattern(PatternKind::StuckAgents, vec!["t1".into()]),
            make_pattern(PatternKind::TestFlakiness, vec!["t2".into()]),
        ];
        let tasks = generate_deduplicated_remediation_tasks(&patterns, &config, &queue);
        // StuckAgents is deduplicated, only TestFlakiness remains.
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].id.starts_with("tqm-test-flaky-"));
    }

    #[test]
    fn dedup_allows_different_kinds() {
        let config = TQMConfig {
            remediation_enabled: true,
            ..TQMConfig::default()
        };
        let queue = crate::taskqueue::TaskQueue::from_tasks(vec![]);

        let patterns = vec![
            make_pattern(PatternKind::StuckAgents, vec!["t1".into()]),
            make_pattern(PatternKind::TestFlakiness, vec!["t2".into()]),
        ];
        let tasks = generate_deduplicated_remediation_tasks(&patterns, &config, &queue);
        assert_eq!(tasks.len(), 2);
    }

    // -----------------------------------------------------------------------
    // analyze_incremental tests
    // -----------------------------------------------------------------------

    #[test]
    fn incremental_detects_new_pattern() {
        let mut result = empty_result();
        result.tasks_attempted = 3;
        result.run_results = vec![
            run_result("t1", "Decomposed"),
            run_result("t1", "Decomposed"),
            run_result("t1", "Decomposed"),
        ];
        let config = TQMConfig::default();
        let previous: Vec<DetectedPattern> = vec![];

        let patterns = analyze_incremental(&result, &config, &previous);
        assert!(
            patterns
                .iter()
                .any(|p| p.kind == PatternKind::DecompositionLoop)
        );
    }

    #[test]
    fn incremental_skips_already_detected_kind() {
        let mut result = empty_result();
        result.tasks_attempted = 3;
        result.run_results = vec![
            run_result("t1", "Decomposed"),
            run_result("t1", "Decomposed"),
            run_result("t1", "Decomposed"),
        ];
        let config = TQMConfig::default();
        // Pretend DecompositionLoop was already detected.
        let previous = vec![make_pattern(
            PatternKind::DecompositionLoop,
            vec!["t1".into()],
        )];

        let patterns = analyze_incremental(&result, &config, &previous);
        assert!(
            !patterns
                .iter()
                .any(|p| p.kind == PatternKind::DecompositionLoop)
        );
    }

    #[test]
    fn incremental_rejection_rate_skipped_when_sample_too_small() {
        let mut result = empty_result();
        result.tasks_attempted = 4; // below the min-sample guard of 5
        result.run_results = vec![
            run_result("t1", "ArchitectRejected"),
            run_result("t2", "ArchitectRejected"),
            run_result("t3", "ArchitectRejected"),
            run_result("t4", "ArchitectRejected"),
        ];
        let config = TQMConfig::default();
        let previous: Vec<DetectedPattern> = vec![];

        let patterns = analyze_incremental(&result, &config, &previous);
        assert!(
            !patterns
                .iter()
                .any(|p| p.kind == PatternKind::ArchitectRejectionRate)
        );
    }

    #[test]
    fn incremental_rejection_rate_fires_with_sufficient_sample() {
        let mut result = empty_result();
        result.tasks_attempted = 6;
        result.run_results = vec![
            run_result("t1", "ArchitectRejected"),
            run_result("t2", "ArchitectRejected"),
            run_result("t3", "ArchitectRejected"),
            run_result("t4", "PrCreated"),
            run_result("t5", "PrCreated"),
            run_result("t6", "PrCreated"),
        ];
        let config = TQMConfig::default(); // threshold 0.5
        let previous: Vec<DetectedPattern> = vec![];

        let patterns = analyze_incremental(&result, &config, &previous);
        assert!(
            patterns
                .iter()
                .any(|p| p.kind == PatternKind::ArchitectRejectionRate)
        );
    }

    // -----------------------------------------------------------------------
    // Boundary violation tests
    // -----------------------------------------------------------------------

    fn run_result_with_error(task_id: &str, status: &str, error: &str) -> TaskRunResult {
        TaskRunResult {
            task_id: task_id.into(),
            status: status.into(),
            cost: 0.5,
            tokens: 1000,
            pr_url: None,
            error: Some(error.into()),
            outcome_context: None,
        }
    }

    #[test]
    fn boundary_violation_detected_on_first_occurrence() {
        let mut result = empty_result();
        result.tasks_attempted = 1;
        result.run_results = vec![run_result("t1", "BoundaryViolation")];
        let patterns = analyze(&result, &TQMConfig::default());
        let boundary = patterns
            .iter()
            .find(|p| p.kind == PatternKind::BoundaryViolation);
        assert!(boundary.is_some());
        let b = boundary.unwrap();
        assert_eq!(b.severity, "warning");
        assert_eq!(b.occurrences, 1);
        assert_eq!(b.affected_tasks, vec!["t1"]);
    }

    #[test]
    fn boundary_violation_from_implementation_failed_with_protected_path() {
        let mut result = empty_result();
        result.tasks_attempted = 1;
        result.run_results = vec![run_result_with_error(
            "t1",
            "ImplementationFailed",
            "PROTECTED PATH — cannot modify 'crates/kernel/src/outcome.rs'. This path is protected by project policy.",
        )];
        let patterns = analyze(&result, &TQMConfig::default());
        let boundary = patterns
            .iter()
            .find(|p| p.kind == PatternKind::BoundaryViolation);
        assert!(boundary.is_some());
        assert_eq!(boundary.unwrap().affected_tasks, vec!["t1"]);
    }

    #[test]
    fn boundary_violation_not_detected_for_normal_failure() {
        let mut result = empty_result();
        result.tasks_attempted = 1;
        result.run_results = vec![run_result_with_error(
            "t1",
            "ImplementationFailed",
            "compilation error in foo.rs",
        )];
        let patterns = analyze(&result, &TQMConfig::default());
        assert!(
            !patterns
                .iter()
                .any(|p| p.kind == PatternKind::BoundaryViolation)
        );
    }

    #[test]
    fn boundary_violation_remediation_task_generated() {
        let config = TQMConfig::default();
        let patterns = vec![make_pattern(
            PatternKind::BoundaryViolation,
            vec!["t1".into()],
        )];
        let tasks = generate_remediation_tasks(&patterns, &config);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].id.starts_with("tqm-boundary-"));
        assert!(tasks[0].objective.contains("boundary violations"));
        assert!(tasks[0].objective.contains("t1"));
    }

    #[test]
    fn boundary_violation_incremental_fires_once() {
        let mut result = empty_result();
        result.tasks_attempted = 1;
        result.run_results = vec![run_result("t1", "BoundaryViolation")];
        let config = TQMConfig::default();

        // First call: should detect the pattern.
        let first = analyze_incremental(&result, &config, &[]);
        assert!(
            first
                .iter()
                .any(|p| p.kind == PatternKind::BoundaryViolation)
        );

        // Second call with previous patterns: should NOT re-fire.
        let second = analyze_incremental(&result, &config, &first);
        assert!(
            !second
                .iter()
                .any(|p| p.kind == PatternKind::BoundaryViolation)
        );
    }

    #[test]
    fn config_backward_compat_without_remediation_in_run() {
        // Configs that still have the old remediation_in_run field should still parse
        // (serde ignores unknown fields with deny_unknown_fields not set).
        let json = r#"{
            "decomposition_loop_threshold": 3,
            "scope_creep_threshold": 3,
            "model_degradation_threshold": 3,
            "stuck_agents_threshold": 3,
            "test_flakiness_threshold": 2,
            "architect_rejection_rate_threshold": 0.5,
            "budget_pressure_threshold": 0.9,
            "provider_failure_threshold": 3,
            "remediation_enabled": true,
            "remediation_in_run": true,
            "remediation_priority": 5
        }"#;
        let parsed: TQMConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.remediation_enabled);
        assert_eq!(parsed.remediation_priority, 5);
    }
}
