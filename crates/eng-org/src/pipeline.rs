use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use glitchlab_kernel::agent::{Agent, AgentContext, AgentMetadata, AgentOutput};
use glitchlab_kernel::budget::BudgetSummary;
use glitchlab_kernel::governance::BoundaryEnforcer;
use glitchlab_kernel::outcome::OutcomeContext;
use glitchlab_kernel::pipeline::{
    EventKind, PipelineContext, PipelineEvent, PipelineResult, PipelineStatus,
};
use glitchlab_kernel::tool::ToolPolicy;
use glitchlab_memory::history::{EventsSummary, HistoryBackend, HistoryEntry};
use tokio::process::Command;
use tracing::{info, warn};

use serde::{Deserialize, Serialize};

use crate::agents::RouterRef;
use crate::agents::architect::{ArchitectReviewAgent, ArchitectTriageAgent};
use crate::agents::archivist::ArchivistAgent;
use crate::agents::claude_code::{ClaudeCodeConfig, ClaudeCodeImplementer};
use crate::agents::debugger::DebuggerAgent;
use crate::agents::implementer::ImplementerAgent;
use crate::agents::planner::PlannerAgent;
use crate::agents::release::ReleaseAgent;
use crate::agents::security::SecurityAgent;
use crate::config::{EngConfig, TaskSize};
use crate::indexer;
use crate::tools::ToolDispatcher;
use crate::workspace::Workspace;

// ---------------------------------------------------------------------------
// ImplementerEfficiency — token usage tracking
// ---------------------------------------------------------------------------

/// Tracks implementer tool usage efficiency for a single pipeline run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImplementerEfficiency {
    /// Total tool calls made by the implementer.
    pub total_tool_calls: u32,
    /// Reads that returned cached content (files already in context).
    pub redundant_reads: u32,
    /// Number of `list_files` calls made.
    pub list_files_calls: u32,
}

// ---------------------------------------------------------------------------
// InterventionHandler — human-in-the-loop gate
// ---------------------------------------------------------------------------

/// Handle human intervention gates (plan review, PR confirmation, etc.).
///
/// Implementers decide how to present the decision to a human
/// (CLI prompt, web UI, Slack, etc.) and return whether to proceed.
pub trait InterventionHandler: Send + Sync {
    /// Ask the human to approve or reject a decision point.
    /// Returns `true` to proceed, `false` to abort.
    fn request_approval(
        &self,
        gate: &str,
        summary: &str,
        data: &serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>>;
}

/// Auto-approves all intervention gates (for CI/autonomous mode).
pub struct AutoApproveHandler;

impl InterventionHandler for AutoApproveHandler {
    fn request_approval(
        &self,
        _gate: &str,
        _summary: &str,
        _data: &serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async { true })
    }
}

// ---------------------------------------------------------------------------
// ExternalOps — abstraction over shell-out calls
// ---------------------------------------------------------------------------

/// Abstraction over external tool invocations (gh, bd, cargo fmt, test runner).
///
/// Production uses real subprocesses via [`RealExternalOps`]; tests inject
/// mock implementations to cover all post-commit branches without requiring
/// a real GitHub remote or `bd` binary.
pub trait ExternalOps: Send + Sync {
    /// Run `cargo fmt --all` in the given directory.
    fn cargo_fmt<'a>(
        &'a self,
        worktree: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// Run the project test command. Returns `Ok(())` on pass, `Err(output)` on fail.
    fn run_tests<'a>(
        &'a self,
        cmd: &'a str,
        worktree: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// Create a PR via `gh pr create`. Returns the PR URL.
    fn create_pr<'a>(
        &'a self,
        worktree: &'a Path,
        branch: &'a str,
        base: &'a str,
        title: &'a str,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

    /// Look up an existing PR for a branch. Returns the PR URL.
    fn view_existing_pr<'a>(
        &'a self,
        worktree: &'a Path,
        branch: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

    /// Fetch a PR diff via `gh pr diff <url>`.
    fn pr_diff<'a>(
        &'a self,
        worktree: &'a Path,
        pr_url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

    /// Auto-merge a PR via `gh pr merge --squash --delete-branch`.
    fn merge_pr<'a>(
        &'a self,
        worktree: &'a Path,
        pr_url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

    /// Close a bead via `bd close <task_id>`.
    fn close_bead<'a>(
        &'a self,
        repo_path: &'a Path,
        bd_path: &'a str,
        task_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// Production implementation — wraps real subprocess calls.
pub struct RealExternalOps;

impl ExternalOps for RealExternalOps {
    fn cargo_fmt<'a>(
        &'a self,
        worktree: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let _ = Command::new("cargo")
                .args(["fmt", "--all"])
                .current_dir(worktree)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
            Ok(())
        })
    }

    fn run_tests<'a>(
        &'a self,
        cmd: &'a str,
        worktree: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let parts: Vec<&str> = cmd.split_whitespace().collect();
            if parts.is_empty() {
                return Ok(());
            }

            let output = Command::new(parts[0])
                .args(&parts[1..])
                .current_dir(worktree)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .map_err(|e| format!("failed to run tests: {e}"))?;

            if output.status.success() {
                Ok(())
            } else {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(format!("{stdout}\n{stderr}"))
            }
        })
    }

    fn create_pr<'a>(
        &'a self,
        worktree: &'a Path,
        branch: &'a str,
        base: &'a str,
        title: &'a str,
        body: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let output = Command::new("gh")
                .args([
                    "pr", "create", "--title", title, "--body", body, "--base", base, "--head",
                    branch,
                ])
                .current_dir(worktree)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .map_err(|e| format!("gh pr create: {e}"))?;

            if output.status.success() {
                let pr_url = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let pr_number = pr_url.split('/').next_back().unwrap_or("unknown");
                info!(
                    "pr_created: Successfully created PR {} ({})",
                    pr_number, pr_url
                );
                Ok(pr_url)
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(format!("gh pr create failed: {stderr}"))
            }
        })
    }

    fn view_existing_pr<'a>(
        &'a self,
        worktree: &'a Path,
        branch: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let output = Command::new("gh")
                .args(["pr", "view", branch, "--json", "url", "-q", ".url"])
                .current_dir(worktree)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .map_err(|e| format!("gh pr view: {e}"))?;

            if output.status.success() {
                let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if url.is_empty() {
                    Err("gh pr view returned empty URL".to_string())
                } else {
                    Ok(url)
                }
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(format!("gh pr view failed: {stderr}"))
            }
        })
    }

    fn pr_diff<'a>(
        &'a self,
        worktree: &'a Path,
        pr_url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let output = Command::new("gh")
                .args(["pr", "diff", pr_url])
                .current_dir(worktree)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .map_err(|e| format!("gh pr diff: {e}"))?;

            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(format!("gh pr diff failed: {stderr}"))
            }
        })
    }

    fn merge_pr<'a>(
        &'a self,
        worktree: &'a Path,
        pr_url: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let output = Command::new("gh")
                .args(["pr", "merge", pr_url, "--squash", "--delete-branch"])
                .current_dir(worktree)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .map_err(|e| format!("failed to run gh pr merge: {e}"))?;

            if output.status.success() {
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(format!("gh pr merge failed: {stderr}"))
            }
        })
    }

    fn close_bead<'a>(
        &'a self,
        repo_path: &'a Path,
        bd_path: &'a str,
        task_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let output = Command::new(bd_path)
                .args(["close", task_id])
                .current_dir(repo_path)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .map_err(|e| format!("failed to run bd close: {e}"))?;

            if output.status.success() {
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(format!("bd close failed: {stderr}"))
            }
        })
    }
}

// ---------------------------------------------------------------------------
// EngineeringPipeline
// ---------------------------------------------------------------------------

/// Full engineering pipeline:
/// Plan -> Implement -> Test/Debug -> Security -> Release -> Archive -> Commit -> PR.
pub struct EngineeringPipeline {
    router: RouterRef,
    config: EngConfig,
    handler: Arc<dyn InterventionHandler>,
    history: Arc<dyn HistoryBackend>,
    ops: Arc<dyn ExternalOps>,
}

impl EngineeringPipeline {
    pub fn new(
        router: RouterRef,
        config: EngConfig,
        handler: Arc<dyn InterventionHandler>,
        history: Arc<dyn HistoryBackend>,
        ops: Arc<dyn ExternalOps>,
    ) -> Self {
        Self {
            router,
            config,
            handler,
            history,
            ops,
        }
    }

    /// Run the full engineering pipeline for a task.
    /// Guarantees workspace cleanup even on failure.
    pub async fn run(
        &self,
        task_id: &str,
        objective: &str,
        repo_path: &Path,
        base_branch: &str,
        previous_attempts: &[OutcomeContext],
    ) -> PipelineResult {
        self.run_with_depth(
            task_id,
            objective,
            repo_path,
            base_branch,
            previous_attempts,
            0,
        )
        .await
    }

    /// Run the pipeline with decomposition depth context.
    ///
    /// Sub-tasks (depth > 0) skip the planner — they already have a plan
    /// from the parent task and go straight to triage + implement.
    pub async fn run_with_depth(
        &self,
        task_id: &str,
        objective: &str,
        repo_path: &Path,
        base_branch: &str,
        previous_attempts: &[OutcomeContext],
        decomposition_depth: u32,
    ) -> PipelineResult {
        info!(task_id, decomposition_depth, "pipeline starting");

        let timeout = Duration::from_secs(self.config.limits.max_pipeline_duration_secs);

        let mut workspace = Workspace::new(repo_path, task_id, &self.config.workspace.worktree_dir);

        let result = match tokio::time::timeout(
            timeout,
            self.run_stages(
                task_id,
                objective,
                repo_path,
                base_branch,
                &mut workspace,
                previous_attempts,
                decomposition_depth,
            ),
        )
        .await
        {
            Ok(r) => r,
            Err(_elapsed) => {
                warn!(
                    task_id,
                    timeout_secs = timeout.as_secs(),
                    "pipeline timed out"
                );
                PipelineResult {
                    status: PipelineStatus::TimedOut,
                    stage_outputs: HashMap::new(),
                    events: vec![],
                    budget: self.router.budget_summary().await,
                    pr_url: None,
                    branch: None,
                    error: Some(format!(
                        "pipeline exceeded wall-clock timeout of {}s",
                        timeout.as_secs()
                    )),
                    outcome_context: None,
                }
            }
        };

        // Always cleanup workspace.
        if let Err(e) = workspace.cleanup().await {
            warn!(task_id, error = %e, "workspace cleanup failed");
        }

        // Record history.
        let entry = build_history_entry(task_id, &result);
        if let Err(e) = self.history.record(&entry).await {
            warn!(task_id, error = %e, "failed to record history");
        }

        info!(task_id, status = ?result.status, "pipeline complete");
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_stages(
        &self,
        task_id: &str,
        objective: &str,
        repo_path: &Path,
        base_branch: &str,
        workspace: &mut Workspace,
        previous_attempts: &[OutcomeContext],
        decomposition_depth: u32,
    ) -> PipelineResult {
        let mut ctx = PipelineContext::new(AgentContext {
            task_id: task_id.into(),
            objective: objective.into(),
            repo_path: repo_path.to_string_lossy().into(),
            working_dir: String::new(),
            constraints: vec![],
            acceptance_criteria: vec![],
            risk_level: "low".into(),
            file_context: HashMap::new(),
            previous_output: serde_json::Value::Null,
            extra: HashMap::new(),
        });

        // --- Stage 1: Create workspace ---
        let wt_path = match workspace.create(base_branch).await {
            Ok(p) => p.to_path_buf(),
            Err(e) => return self.fail(ctx, PipelineStatus::Error, e.to_string()).await,
        };
        ctx.agent_context.working_dir = wt_path.to_string_lossy().into();
        self.emit(
            &mut ctx,
            EventKind::WorkspaceCreated,
            serde_json::json!({ "path": wt_path.display().to_string() }),
        );

        // --- Stage 2: Index + context enrichment ---
        let (repo_context, indexed_files, codebase_knowledge) =
            match indexer::build_index(repo_path).await {
                Ok(index) => {
                    let ctx_str = index.to_agent_context(100);
                    let files = index.files.clone();
                    let knowledge = indexer::build_codebase_knowledge(&index, Path::new(repo_path));
                    (ctx_str, files, knowledge)
                }
                Err(e) => {
                    warn!(task_id, error = %e, "indexer failed, continuing");
                    (String::new(), Vec::new(), String::new())
                }
            };

        let failure_context = self.history.failure_context(5).await.unwrap_or_default();

        let mut enriched = format!("## Task\n\n{objective}");
        if !repo_context.is_empty() {
            enriched.push_str("\n\n");
            enriched.push_str(&repo_context);
        }
        if !self.config.boundaries.protected_paths.is_empty() {
            enriched.push_str("\n\n## Protected Paths\n\n");
            enriched.push_str(
                "The following paths are protected by project policy. \
                 If the task requires changes under any of these, \
                 set `requires_core_change` to true.\n\n",
            );
            for p in &self.config.boundaries.protected_paths {
                enriched.push_str(&format!("- `{p}`\n"));
            }
        }

        ctx.agent_context.objective = enriched;

        // Store failure context separately in extra (independently droppable
        // by ContextAssembler, rather than baked into the objective string).
        if !failure_context.is_empty() {
            ctx.agent_context.extra.insert(
                "failure_history".into(),
                serde_json::Value::String(failure_context),
            );
        }

        // Inject structured previous attempt contexts from the AttemptTracker
        // so the planner can learn from prior failures on this same task.
        if !previous_attempts.is_empty()
            && let Ok(val) = serde_json::to_value(previous_attempts)
        {
            ctx.agent_context
                .extra
                .insert("previous_attempts".into(), val);
        }

        // Inject codebase knowledge early so ALL agents (planner included) get it.
        // Prefer a hand-curated file if present; fall back to auto-generated.
        let repo_root = Path::new(&ctx.agent_context.repo_path);
        let knowledge_path = repo_root.join(".glitchlab/codebase-context.md");
        let knowledge = if knowledge_path.exists() {
            tokio::fs::read_to_string(&knowledge_path)
                .await
                .unwrap_or_default()
        } else {
            codebase_knowledge.clone()
        };
        if !knowledge.is_empty() {
            ctx.agent_context.extra.insert(
                "codebase_knowledge".into(),
                serde_json::Value::String(knowledge),
            );
        }

        // Feed relevant source files into agent context.
        ctx.agent_context.file_context =
            read_relevant_files(repo_path, objective, &indexed_files).await;

        // Inject file-size metadata so the planner can reason about token cost.
        let file_sizes = compute_file_sizes(&ctx.agent_context.file_context);
        if !file_sizes.is_empty() {
            let size_hint = format_file_size_hint(&file_sizes);
            ctx.agent_context.constraints.push(size_hint);
        }

        // --- Stage 2b: Pre-planner boundary scan (deterministic, zero LLM calls) ---
        // If the objective itself mentions file paths under protected prefixes,
        // reject immediately. This catches the common case where the task is
        // "modify <protected file>" and saves all downstream LLM budget.
        if !self.config.boundaries.protected_paths.is_empty() {
            let objective_paths = extract_file_paths_from_text(objective);
            if !objective_paths.is_empty() {
                let enforcer = BoundaryEnforcer::new(
                    self.config
                        .boundaries
                        .protected_paths
                        .iter()
                        .map(PathBuf::from)
                        .collect(),
                );
                let violations = enforcer.check(&objective_paths);
                if !violations.is_empty() {
                    let paths: Vec<String> =
                        violations.iter().map(|p| p.display().to_string()).collect();
                    return self
                        .fail(
                            ctx,
                            PipelineStatus::BoundaryViolation,
                            format!(
                                "objective references protected path(s): {}",
                                paths.join(", ")
                            ),
                        )
                        .await;
                }
            }
        }

        // --- Stage 3: Plan ---
        // Sub-tasks (depth > 0) already have a plan from their parent.
        // Skip the planner entirely — synthesize a minimal plan from the
        // objective and go straight to implementation. This saves 6-8k
        // tokens per sub-task.
        let is_sub_task = decomposition_depth > 0;
        // Leaf beads (id contains '.') are already human-decomposed children
        // of an epic.  Don't allow the planner to re-decompose them.
        let is_leaf_bead = task_id.contains('.') && decomposition_depth == 0;
        let mut plan_output;

        if is_sub_task {
            info!(
                task_id,
                decomposition_depth, "sub-task — skipping planner, using objective as plan"
            );
            // Extract file hints from the objective (appended by orchestrator).
            let files: Vec<String> = ctx
                .agent_context
                .objective
                .lines()
                .filter(|l| l.starts_with("Files from parent task:"))
                .flat_map(|l| {
                    l.trim_start_matches("Files from parent task:")
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                })
                .collect();

            // --- Sub-task budget pre-flight ---
            // Sub-tasks skip the planner, so they bypass the budget estimation
            // gate. Check here: if the files are too large for the budget, fail
            // fast instead of wasting tokens on a doomed implementation.
            let estimated = estimate_plan_tokens(repo_path, &files, 1).await;
            let budget_limit = self.config.limits.max_tokens_per_task as usize;
            // Sub-tasks use a higher threshold (90%) than root tasks (70%)
            // because the estimator is deliberately pessimistic and sub-tasks
            // are already narrowly scoped by decomposition.
            if estimated > (budget_limit * 90 / 100) {
                warn!(
                    task_id,
                    estimated,
                    budget_limit,
                    "sub-task files exceed budget pre-flight — failing fast"
                );
                return self
                    .fail(
                        ctx,
                        PipelineStatus::PlanFailed,
                        format!(
                            "sub-task estimated at ~{estimated} tokens (budget: {budget_limit}). \
                             Parent decomposition produced a sub-task too large to implement. \
                             Files: {}",
                            files.join(", ")
                        ),
                    )
                    .await;
            }

            let synthetic_plan = serde_json::json!({
                "steps": [{"description": objective}],
                "files_likely_affected": files,
                "estimated_complexity": "small",
                "requires_core_change": false,
            });
            plan_output = AgentOutput {
                data: synthetic_plan.clone(),
                metadata: AgentMetadata {
                    agent: "planner".into(),
                    model: "synthetic".into(),
                    tokens: 0,
                    cost: 0.0,
                    latency_ms: 0,
                },
                parse_error: false,
            };
            ctx.current_stage = Some("plan".into());
            self.emit(&mut ctx, EventKind::PlanCreated, plan_output.data.clone());
            ctx.stage_outputs.insert("plan".into(), plan_output.clone());
        } else {
            ctx.current_stage = Some("plan".into());
            if is_leaf_bead {
                ctx.agent_context.constraints.push(
                    "This task is a leaf work item (already decomposed by the project owner). \
                     Do NOT decompose further. Plan a single implementation pass."
                        .into(),
                );
            }
            let planner = PlannerAgent::new(Arc::clone(&self.router));
            plan_output = match planner.execute(&ctx.agent_context).await {
                Ok(o) => o,
                Err(e) => {
                    return self
                        .fail(ctx, PipelineStatus::PlanFailed, e.to_string())
                        .await;
                }
            };
            self.emit(&mut ctx, EventKind::PlanCreated, plan_output.data.clone());
            ctx.stage_outputs.insert("plan".into(), plan_output.clone());

            // --- Stage 3b: Check for decomposition ---
            // Guard: strip spurious decomposition from tasks that don't need it.
            // The planner sometimes decomposes trivial/small tasks, or medium
            // tasks that touch ≤3 files (within a single implementer pass).
            let complexity = plan_output.data["estimated_complexity"]
                .as_str()
                .unwrap_or("unknown");
            let file_count = plan_output.data["files_likely_affected"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);

            if (complexity == "trivial"
                || complexity == "small"
                || (complexity == "medium" && file_count <= 3)
                || is_leaf_bead)
                && plan_output
                    .data
                    .get("decomposition")
                    .is_some_and(|d| d.is_array())
            {
                warn!(
                    task_id,
                    complexity, file_count, is_leaf_bead, "stripping spurious decomposition"
                );
                plan_output
                    .data
                    .as_object_mut()
                    .map(|o| o.remove("decomposition"));
                // Update the stored plan output so downstream code sees the stripped version.
                ctx.stage_outputs.insert("plan".into(), plan_output.clone());
            }

            if plan_output
                .data
                .get("decomposition")
                .is_some_and(|d| d.is_array())
            {
                info!(
                    task_id,
                    "planner decomposed task into sub-tasks, returning early"
                );
                return PipelineResult {
                    status: PipelineStatus::Decomposed,
                    stage_outputs: ctx.stage_outputs,
                    events: ctx.events,
                    budget: self.router.budget_summary().await,
                    pr_url: None,
                    branch: None,
                    error: None,
                    outcome_context: None,
                };
            }

            // --- Stage 3b2: Budget estimation gate ---
            // Estimate whether the plan fits within the task's token budget.
            // If it would exceed 80%, force the planner to decompose.
            //
            // Skip when Claude Code is the implementer — it manages its own
            // 200K context window, so the native token estimation doesn't apply.
            if !self.config.pipeline.use_claude_code_implementer {
                let plan_files: Vec<String> = plan_output.data["files_likely_affected"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let step_count = plan_output.data["steps"]
                    .as_array()
                    .map(|a| a.len())
                    .unwrap_or(1);
                let estimated = estimate_plan_tokens(repo_path, &plan_files, step_count).await;
                let budget = self.config.limits.max_tokens_per_task as usize;

                // 90% threshold — the estimator is deliberately pessimistic, so the
                // gate catches plans that would clearly blow budget while allowing
                // borderline cases to attempt execution.
                if estimated > (budget * 90 / 100) {
                    warn!(
                        task_id,
                        estimated,
                        budget,
                        "plan estimated to exceed 90% of budget — forcing decomposition"
                    );
                    ctx.agent_context.previous_output = serde_json::json!({
                        "original_plan": plan_output.data,
                        "instruction": format!(
                            "This plan would consume ~{estimated} tokens (budget: {budget}). \
                             Decompose into smaller sub-tasks. Each sub-task should read only \
                             the specific sections of files it needs (use line-range hints)."
                        ),
                    });
                    let replan = PlannerAgent::new(Arc::clone(&self.router));
                    match replan.execute(&ctx.agent_context).await {
                        Ok(new_plan) => {
                            plan_output = new_plan;
                            self.emit(&mut ctx, EventKind::PlanCreated, plan_output.data.clone());
                            ctx.stage_outputs.insert("plan".into(), plan_output.clone());
                            // Check if replan decomposed
                            if plan_output
                                .data
                                .get("decomposition")
                                .is_some_and(|d| d.is_array() && !d.as_array().unwrap().is_empty())
                            {
                                return PipelineResult {
                                    status: PipelineStatus::Decomposed,
                                    stage_outputs: ctx.stage_outputs,
                                    events: ctx.events,
                                    budget: self.router.budget_summary().await,
                                    pr_url: None,
                                    branch: None,
                                    error: None,
                                    outcome_context: None,
                                };
                            }
                        }
                        Err(e) => {
                            warn!(
                                task_id,
                                error = %e,
                                "budget-aware replan failed, continuing with original"
                            );
                        }
                    }
                }
            }
        }

        // --- Short-circuit eligibility check ---
        // Sub-tasks are always short-circuit eligible (parent already planned).
        let use_short_circuit = is_sub_task
            || (self.config.pipeline.short_circuit_enabled
                && is_short_circuit_eligible(
                    &plan_output.data,
                    self.config.pipeline.short_circuit_max_files,
                ));

        // Task size — defaults to M, overwritten by triage when it runs.
        let mut task_size = TaskSize::M;

        if use_short_circuit {
            info!(
                task_id,
                ?task_size,
                "short-circuit eligible — skipping triage, security, release, archivist"
            );
        }

        // --- Stage 3c: Architect triage (skipped on short circuit) ---
        if !use_short_circuit {
            ctx.current_stage = Some("architect_triage".into());
            ctx.agent_context.previous_output = plan_output.data.clone();

            // Re-load affected files so triage can check if code already exists.
            let triage_files: Vec<String> = plan_output.data["files_likely_affected"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            for file in &triage_files {
                let full_path = repo_path.join(file);
                if let Ok(content) = tokio::fs::read_to_string(&full_path).await {
                    let truncated = if content.len() > 8192 {
                        format!("{}...\n[truncated]", &content[..8192])
                    } else {
                        content
                    };
                    ctx.agent_context
                        .file_context
                        .insert(file.clone(), truncated);
                }
            }

            let triage_agent = ArchitectTriageAgent::new(Arc::clone(&self.router));
            let triage_output = match triage_agent.execute(&ctx.agent_context).await {
                Ok(o) => o,
                Err(e) => {
                    warn!(task_id, error = %e, "architect triage failed, continuing");
                    fallback_output(
                        "architect_triage",
                        serde_json::json!({
                            "verdict": "proceed",
                            "task_size": "M",
                            "sizing_rationale": "default — triage agent failed",
                            "confidence": 0.0,
                            "reasoning": "triage agent failed",
                            "evidence": [],
                            "architectural_notes": "",
                            "suggested_changes": []
                        }),
                    )
                }
            };

            let triage_verdict = triage_output.data["verdict"]
                .as_str()
                .unwrap_or("proceed")
                .to_string();

            // Extract task size from triage (default M if missing).
            let task_size_str = triage_output.data["task_size"].as_str().unwrap_or("M");
            task_size = match task_size_str {
                "S" => TaskSize::S,
                "L" => TaskSize::L,
                "XL" => TaskSize::XL,
                _ => TaskSize::M,
            };

            self.emit(
                &mut ctx,
                EventKind::ArchitectTriage,
                triage_output.data.clone(),
            );
            ctx.stage_outputs
                .insert("architect_triage".into(), triage_output);

            // XL → force needs_refinement (even if verdict was "proceed").
            if task_size == TaskSize::XL && triage_verdict != "already_done" {
                info!(task_id, "triage sized XL — forcing decomposition");

                let triage_data = ctx
                    .stage_outputs
                    .get("architect_triage")
                    .map(|o| o.data.clone())
                    .unwrap_or_default();

                ctx.agent_context.previous_output = serde_json::json!({
                    "original_plan": plan_output.data,
                    "triage_feedback": triage_data,
                    "instruction": "Task is too large (XL). Decompose into S/M/L sub-tasks. Each sub-task must touch at most 2 files.",
                });

                let xl_planner = PlannerAgent::new(Arc::clone(&self.router));
                let replan_output = match xl_planner.execute(&ctx.agent_context).await {
                    Ok(o) => o,
                    Err(e) => {
                        return self
                            .fail(
                                ctx,
                                PipelineStatus::PlanFailed,
                                format!("XL replan failed: {e}"),
                            )
                            .await;
                    }
                };
                self.emit(&mut ctx, EventKind::PlanCreated, replan_output.data.clone());
                ctx.stage_outputs
                    .insert("plan".into(), replan_output.clone());
                plan_output = replan_output;

                // XL replan should produce a decomposition.
                if let Some(decomposition) = plan_output.data.get("decomposition")
                    && decomposition.is_array()
                    && !decomposition.as_array().unwrap().is_empty()
                {
                    let budget = self.router.budget_summary().await;
                    return PipelineResult {
                        status: PipelineStatus::Decomposed,
                        stage_outputs: ctx.stage_outputs,
                        events: ctx.events,
                        budget,
                        pr_url: None,
                        branch: None,
                        error: None,
                        outcome_context: None,
                    };
                }
                // If replan didn't decompose, fall through with the new plan.
            }

            if triage_verdict == "already_done" {
                info!(task_id, "architect triage: work already done, skipping");
                let budget = self.router.budget_summary().await;
                return PipelineResult {
                    status: PipelineStatus::AlreadyDone,
                    stage_outputs: ctx.stage_outputs,
                    events: ctx.events,
                    budget,
                    pr_url: None,
                    branch: None,
                    error: None,
                    outcome_context: None,
                };
            }

            if triage_verdict == "needs_refinement" {
                info!(task_id, "architect triage: refinement needed, replanning");

                let triage_data = ctx
                    .stage_outputs
                    .get("architect_triage")
                    .map(|o| o.data.clone())
                    .unwrap_or_default();

                ctx.agent_context.previous_output = serde_json::json!({
                    "original_plan": plan_output.data,
                    "triage_feedback": triage_data,
                    "instruction": "The architect reviewed your plan and requested refinements. Revise the plan addressing every issue in triage_feedback.suggested_changes.",
                });

                // Re-invoke planner (max 1 replan — no recursion).
                let replan_planner = PlannerAgent::new(Arc::clone(&self.router));
                let replan_output = match replan_planner.execute(&ctx.agent_context).await {
                    Ok(o) => o,
                    Err(e) => {
                        return self
                            .fail(
                                ctx,
                                PipelineStatus::PlanFailed,
                                format!("replan failed: {e}"),
                            )
                            .await;
                    }
                };
                self.emit(&mut ctx, EventKind::PlanCreated, replan_output.data.clone());
                ctx.stage_outputs
                    .insert("plan".into(), replan_output.clone());
                plan_output = replan_output;

                // Re-check for decomposition (same logic as Stage 3b).
                if let Some(decomposition) = plan_output.data.get("decomposition")
                    && decomposition.is_array()
                    && !decomposition.as_array().unwrap().is_empty()
                {
                    let budget = self.router.budget_summary().await;
                    return PipelineResult {
                        status: PipelineStatus::Decomposed,
                        stage_outputs: ctx.stage_outputs,
                        events: ctx.events,
                        budget,
                        pr_url: None,
                        branch: None,
                        error: None,
                        outcome_context: None,
                    };
                }
            }
        }

        // XL tasks that weren't decomposed above must not reach the implementer.
        if task_size == TaskSize::XL {
            return self
                .fail(
                    ctx,
                    PipelineStatus::PlanFailed,
                    "XL task could not be decomposed — cannot proceed to implementation".into(),
                )
                .await;
        }

        // Resize budget: add overhead already consumed (planner/triage) to the
        // task-size ceiling so only implementer tokens count against the limit.
        let overhead = self.router.budget_summary().await.total_tokens;
        let effective_limit = task_size.max_tokens() + overhead;
        self.router.resize_budget(effective_limit).await;
        info!(
            task_id,
            ?task_size,
            overhead,
            effective_limit,
            "budget sized by triage (overhead excluded)"
        );

        // Strip repo context from objective — the planner already consumed it;
        // downstream agents (implementer, debugger, etc.) don't need the full
        // repo index occupying their context windows.
        ctx.agent_context.objective = format!("## Task\n\n{objective}");

        // Pre-seed implementer with planner-identified files from the worktree.
        let planner_files: Vec<String> = plan_output.data["files_likely_affected"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        ctx.agent_context.file_context.clear();
        let mut preseed_total = 0usize;
        for file in &planner_files {
            if preseed_total >= MAX_TOTAL_BYTES {
                break;
            }
            let full_path = wt_path.join(file);
            if let Ok(content) = tokio::fs::read_to_string(&full_path).await {
                let truncated = if content.len() > MAX_FILE_BYTES {
                    format!("{}...\n[truncated]", &content[..MAX_FILE_BYTES])
                } else {
                    content
                };
                preseed_total += truncated.len();
                ctx.agent_context
                    .file_context
                    .insert(file.clone(), truncated);
            }
        }

        // Build lightweight Rust module map if this is a Rust project.
        if wt_path.join("Cargo.toml").exists() {
            let module_map = build_rust_module_map(&wt_path, &planner_files);
            if !module_map.is_empty() {
                ctx.agent_context
                    .extra
                    .insert("module_map".into(), serde_json::Value::String(module_map));
            }
        }

        // --- Stage 4: Boundary check ---
        // Extract files from ALL plan fields, not just `files_likely_affected`.
        // This prevents the planner from gaming the top-level list while hiding
        // protected paths inside step details.
        ctx.current_stage = Some("boundary_check".into());
        let files_affected = extract_all_plan_files(&plan_output.data);

        // NOTE: We always enforce boundaries here (allow_core = false).
        // The planner's `requires_core_change` flag is informational only —
        // it tells TQM and the orchestrator *why* the task was rejected, but
        // does NOT bypass enforcement. The only legitimate bypass is the
        // `--allow-core` CLI flag, which clears the protected_paths list
        // before the pipeline runs.
        let enforcer = BoundaryEnforcer::new(
            self.config
                .boundaries
                .protected_paths
                .iter()
                .map(PathBuf::from)
                .collect(),
        );

        if let Err(e) = enforcer.enforce(&files_affected, false) {
            return self
                .fail(ctx, PipelineStatus::BoundaryViolation, e.to_string())
                .await;
        }
        self.emit(
            &mut ctx,
            EventKind::BoundaryChecked,
            serde_json::Value::Null,
        );

        // --- Stage 5: Human gate — plan review ---
        if self.config.intervention.pause_after_plan {
            let summary = format_plan_summary(&plan_output);
            if !self
                .handler
                .request_approval("plan_review", &summary, &plan_output.data)
                .await
            {
                return self
                    .fail(ctx, PipelineStatus::Interrupted, "plan rejected".into())
                    .await;
            }
        }

        // --- Stage 6: Implement ---
        ctx.current_stage = Some("implement".into());
        ctx.agent_context.previous_output = plan_output.data.clone();

        // Select implementer backend: Claude Code CLI or native tool-use loop.
        let use_claude_code = self.config.pipeline.use_claude_code_implementer;
        let (mut impl_output, cache_hits) = if use_claude_code {
            info!(task_id, "using Claude Code implementer backend");
            let cc_config = ClaudeCodeConfig {
                model: self.config.pipeline.claude_code_model.clone(),
                max_budget_usd: self.config.pipeline.claude_code_budget_usd,
                max_turns: (task_size.max_tool_turns() * 3).max(20), // Claude Code turns are cheaper
                claude_bin: "claude".into(),
            };
            let cc_agent = ClaudeCodeImplementer::new(cc_config);
            match cc_agent.execute(&ctx.agent_context).await {
                Ok(o) => {
                    // Record Claude Code's cost in the router budget so
                    // budget_summary() reflects the full pipeline spend.
                    self.router.record_external_cost(o.metadata.cost).await;
                    (o, 0u32)
                }
                Err(e) => {
                    let err_str = e.to_string();
                    let obstacle = glitchlab_kernel::outcome::ObstacleKind::ModelLimitation {
                        model: format!("claude-code/{}", self.config.pipeline.claude_code_model),
                        error_class: err_str.clone(),
                    };
                    return self
                        .fail_with_context(
                            ctx,
                            PipelineStatus::ImplementationFailed,
                            err_str,
                            Some(OutcomeContext {
                                approach: "claude-code implementation".into(),
                                obstacle,
                                discoveries: vec![],
                                recommendation: Some(
                                    "Check claude CLI availability and API key".into(),
                                ),
                                files_explored: vec![],
                            }),
                        )
                        .await;
                }
            }
        } else {
            let impl_tool_policy = ToolPolicy::new(
                self.config.allowed_tools.clone(),
                self.config.blocked_patterns.clone(),
            );
            let impl_dispatcher = ToolDispatcher::new(
                wt_path.clone(),
                impl_tool_policy,
                self.config.boundaries.protected_paths.clone(),
                Duration::from_secs(120),
            );
            // Seed read cache with pre-loaded file contents so the implementer
            // doesn't waste tool turns re-reading files already in context.
            impl_dispatcher.seed_cache(&ctx.agent_context.file_context);
            let mut implementer = ImplementerAgent::new(
                Arc::clone(&self.router),
                impl_dispatcher,
                task_size.max_tool_turns(),
                self.config.limits.max_stuck_turns,
            );
            if use_short_circuit {
                implementer = implementer.without_list_files();
            }
            let hits = implementer.cache_hit_count();
            match implementer.execute(&ctx.agent_context).await {
                Ok(o) => {
                    let cache_hits = implementer.cache_hit_count() - hits;
                    (o, cache_hits)
                }
                Err(e) => {
                    let err_str = e.to_string();
                    let obstacle = if err_str.contains("budget exceeded")
                        || err_str.contains("budget_exceeded")
                    {
                        glitchlab_kernel::outcome::ObstacleKind::ModelLimitation {
                            model: "unknown".into(),
                            error_class: "budget_exceeded".into(),
                        }
                    } else {
                        glitchlab_kernel::outcome::ObstacleKind::ModelLimitation {
                            model: "unknown".into(),
                            error_class: err_str.clone(),
                        }
                    };
                    return self
                        .fail_with_context(
                            ctx,
                            PipelineStatus::ImplementationFailed,
                            err_str,
                            Some(OutcomeContext {
                                approach: "direct implementation".into(),
                                obstacle,
                                discoveries: vec![],
                                recommendation: Some(
                                    "Try a different implementation approach or simpler plan"
                                        .into(),
                                ),
                                files_explored: vec![],
                            }),
                        )
                        .await;
                }
            }
        };
        // Track implementer efficiency.
        let efficiency = ImplementerEfficiency {
            total_tool_calls: impl_output.metadata.tokens.min(u32::MAX as u64) as u32 / 3000, // approximate
            redundant_reads: cache_hits,
            list_files_calls: 0, // TODO: track from dispatcher
        };
        if efficiency.redundant_reads > 0 {
            info!(
                task_id,
                redundant_reads = efficiency.redundant_reads,
                "implementer efficiency: redundant reads avoided by cache"
            );
        }

        self.emit(
            &mut ctx,
            EventKind::ImplementationComplete,
            impl_output.data.clone(),
        );
        ctx.stage_outputs
            .insert("implement".into(), impl_output.clone());

        // Store efficiency data alongside implementation output.
        if let Ok(eff_json) = serde_json::to_value(&efficiency) {
            ctx.stage_outputs.insert(
                "implementer_efficiency".into(),
                AgentOutput {
                    data: eff_json,
                    metadata: AgentMetadata {
                        agent: "efficiency_tracker".into(),
                        model: String::new(),
                        tokens: 0,
                        cost: 0.0,
                        latency_ms: 0,
                    },
                    parse_error: false,
                },
            );
        }

        // Bail early if the implementer got stuck or produced no useful output.
        if impl_output.parse_error
            || impl_output.data.get("stuck").and_then(|v| v.as_bool()) == Some(true)
        {
            let reason = impl_output.data["stuck_reason"]
                .as_str()
                .unwrap_or("parse_error");

            // Route boundary violations to BoundaryViolation status so
            // TQM can detect them without relying on error-string matching.
            if reason == "boundary_violation" {
                return self
                    .fail_with_context(
                        ctx,
                        PipelineStatus::BoundaryViolation,
                        format!("implementer hit protected path: {reason}"),
                        Some(OutcomeContext {
                            approach: "direct implementation".into(),
                            obstacle: glitchlab_kernel::outcome::ObstacleKind::ArchitecturalGap {
                                description: format!("protected path: {reason}"),
                            },
                            discoveries: vec![],
                            recommendation: Some("Decompose to exclude protected paths".into()),
                            files_explored: vec![],
                        }),
                    )
                    .await;
            }

            // Pure parse error (LLM returned garbage) — try model escalation
            // before giving up. Stuck-loop failures are not retryable.
            if reason == "parse_error" {
                // Use the actual raw output from the CLI (if captured) instead
                // of the fallback JSON, so Circuit/TQM can diagnose whether
                // this is an infrastructure issue vs model garbage.
                let raw_snippet: String = impl_output
                    .data
                    .get("_raw_output_preview")
                    .and_then(|v| v.as_str())
                    .map(|s| s.chars().take(200).collect())
                    .unwrap_or_else(|| impl_output.data.to_string().chars().take(200).collect());

                if let Some(escalated_model) = self.router.escalate_model("implementer").await {
                    let original_model = impl_output.metadata.model.clone();
                    warn!(
                        task_id,
                        original_model,
                        escalated_model,
                        "parse error — retrying with escalated model"
                    );

                    // Override the implementer model for the retry.
                    self.router
                        .set_model_override("implementer", escalated_model.clone())
                        .await;

                    // Re-run the implementer with the same context.
                    let retry_dispatcher = ToolDispatcher::new(
                        wt_path.clone(),
                        ToolPolicy::new(
                            self.config.allowed_tools.clone(),
                            self.config.blocked_patterns.clone(),
                        ),
                        self.config.boundaries.protected_paths.clone(),
                        Duration::from_secs(120),
                    );
                    let retry_implementer = ImplementerAgent::new(
                        Arc::clone(&self.router),
                        retry_dispatcher,
                        TaskSize::L.max_tool_turns(),
                        self.config.limits.max_stuck_turns,
                    );
                    let retry_output = retry_implementer.execute(&ctx.agent_context).await;

                    // Clear the override regardless of outcome.
                    self.router.clear_model_override("implementer").await;

                    match retry_output {
                        Ok(output) if !output.parse_error => {
                            info!(task_id, escalated_model, "model escalation retry succeeded");
                            // Replace the original output and continue the pipeline.
                            impl_output = output;
                            ctx.stage_outputs
                                .insert("implement".into(), impl_output.clone());
                            // Fall through to test/debug loop below.
                        }
                        _ => {
                            // Escalation retry also failed.
                            return self
                                .fail_with_context(
                                    ctx,
                                    PipelineStatus::ParseError,
                                    format!(
                                        "implementer failed: parse_error (escalation to {escalated_model} also failed)"
                                    ),
                                    Some(OutcomeContext {
                                        approach: "direct implementation".into(),
                                        obstacle:
                                            glitchlab_kernel::outcome::ObstacleKind::ParseFailure {
                                                model: escalated_model.clone(),
                                                raw_snippet: raw_snippet.clone(),
                                            },
                                        discoveries: vec![],
                                        recommendation: Some(
                                            "Use a different model or simplify the task".into(),
                                        ),
                                        files_explored: vec![],
                                    }),
                                )
                                .await;
                        }
                    }
                } else {
                    // No escalated model available.
                    return self
                        .fail_with_context(
                            ctx,
                            PipelineStatus::ParseError,
                            format!("implementer failed: {reason}"),
                            Some(OutcomeContext {
                                approach: "direct implementation".into(),
                                obstacle: glitchlab_kernel::outcome::ObstacleKind::ParseFailure {
                                    model: impl_output.metadata.model.clone(),
                                    raw_snippet,
                                },
                                discoveries: vec![],
                                recommendation: Some(
                                    "Use a different model or simplify the task".into(),
                                ),
                                files_explored: vec![],
                            }),
                        )
                        .await;
                }
            } else {
                return self
                    .fail_with_context(
                        ctx,
                        PipelineStatus::ImplementationFailed,
                        format!("implementer failed: {reason}"),
                        Some(OutcomeContext {
                            approach: "direct implementation".into(),
                            obstacle: glitchlab_kernel::outcome::ObstacleKind::ModelLimitation {
                                model: impl_output.metadata.model.clone(),
                                error_class: reason.to_string(),
                            },
                            discoveries: vec![],
                            recommendation: Some(
                                "Try a different implementation approach or simpler plan".into(),
                            ),
                            files_explored: vec![],
                        }),
                    )
                    .await;
            }
        }

        // --- Stage 7: Test / debug loop ---
        ctx.current_stage = Some("test".into());
        let test_cmd = self
            .config
            .test_command_override
            .clone()
            .or_else(|| crate::config::detect_test_command(repo_path));
        let mut fix_attempts = 0u32;
        let max_fixes = self.config.limits.max_fix_attempts;

        if let Some(ref cmd) = test_cmd {
            loop {
                match self.ops.run_tests(cmd, &wt_path).await {
                    Ok(()) => {
                        self.emit(
                            &mut ctx,
                            EventKind::TestsPassed,
                            serde_json::json!({ "attempt": fix_attempts + 1 }),
                        );
                        break;
                    }
                    Err(test_output) => {
                        self.emit(
                            &mut ctx,
                            EventKind::TestsFailed,
                            serde_json::json!({
                                "attempt": fix_attempts + 1,
                                "output": truncate(&test_output, 2000),
                            }),
                        );

                        if fix_attempts >= max_fixes {
                            let test_error_snippet = truncate(&test_output, 500);
                            return self
                                .fail_with_context(
                                    ctx,
                                    PipelineStatus::TestsFailed,
                                    format!("tests failing after {max_fixes} fix attempts"),
                                    Some(OutcomeContext {
                                        approach: "direct implementation".into(),
                                        obstacle:
                                            glitchlab_kernel::outcome::ObstacleKind::TestFailure {
                                                attempts: fix_attempts,
                                                last_error: test_error_snippet,
                                            },
                                        discoveries: vec![],
                                        recommendation: Some(
                                            "Check test expectations or simplify the implementation"
                                                .into(),
                                        ),
                                        files_explored: vec![],
                                    }),
                                )
                                .await;
                        }

                        fix_attempts += 1;

                        // Invoke debugger.
                        ctx.current_stage = Some("debug".into());
                        ctx.agent_context.previous_output = serde_json::json!({
                            "test_output": truncate(&test_output, 2000),
                            "implementation": impl_output.data,
                            "fix_attempt": fix_attempts,
                        });

                        let dbg_tool_policy = ToolPolicy::new(
                            self.config.allowed_tools.clone(),
                            self.config.blocked_patterns.clone(),
                        );
                        let dbg_dispatcher = ToolDispatcher::new(
                            wt_path.clone(),
                            dbg_tool_policy,
                            self.config.boundaries.protected_paths.clone(),
                            Duration::from_secs(120),
                        );
                        let debugger = DebuggerAgent::new(
                            Arc::clone(&self.router),
                            dbg_dispatcher,
                            self.config.limits.max_tool_turns,
                            self.config.limits.max_stuck_turns,
                        );
                        let debug_out = match debugger.execute(&ctx.agent_context).await {
                            Ok(o) => o,
                            Err(e) => {
                                return self
                                    .fail(
                                        ctx,
                                        PipelineStatus::TestsFailed,
                                        format!("debugger failed: {e}"),
                                    )
                                    .await;
                            }
                        };

                        self.emit(&mut ctx, EventKind::DebugAttempt, debug_out.data.clone());
                        ctx.stage_outputs
                            .insert(format!("debug_{fix_attempts}"), debug_out.clone());

                        if !debug_out.data["should_retry"].as_bool().unwrap_or(true) {
                            return self
                                .fail(
                                    ctx,
                                    PipelineStatus::TestsFailed,
                                    "debugger recommends not retrying".into(),
                                )
                                .await;
                        }

                        ctx.current_stage = Some("test".into());
                    }
                }
            }
        }

        // --- Stage 10: Security review (skipped on short circuit) ---
        let security_output = if use_short_circuit {
            let synthetic = fallback_output(
                "security",
                serde_json::json!({
                    "verdict": "skipped",
                    "issues": [],
                    "summary": "skipped (short circuit)"
                }),
            );
            ctx.stage_outputs
                .insert("security".into(), synthetic.clone());
            synthetic
        } else {
            ctx.current_stage = Some("security".into());
            let diff = workspace.diff_full(base_branch).await.unwrap_or_default();
            ctx.agent_context.previous_output = serde_json::json!({
                "diff": truncate(&diff, 4000),
                "plan": plan_output.data,
                "implementation": impl_output.data,
            });

            let security_agent = SecurityAgent::new(Arc::clone(&self.router));
            let output = match security_agent.execute(&ctx.agent_context).await {
                Ok(o) => o,
                Err(e) => {
                    warn!(task_id, error = %e, "security review failed");
                    fallback_output(
                        "security",
                        serde_json::json!({
                            "verdict": "warn",
                            "issues": [],
                            "summary": "security agent failed"
                        }),
                    )
                }
            };

            let verdict = output.data["verdict"].as_str().unwrap_or("pass");
            self.emit(&mut ctx, EventKind::SecurityReview, output.data.clone());
            ctx.stage_outputs.insert("security".into(), output.clone());

            if verdict == "block" {
                return self
                    .fail(
                        ctx,
                        PipelineStatus::SecurityBlocked,
                        "security review blocked changes".into(),
                    )
                    .await;
            }
            output
        };

        // --- Stage 10b: CISO risk analysis (skipped on short circuit) ---
        if !use_short_circuit {
            ctx.current_stage = Some("ciso".into());
            let ciso_diff = workspace.diff_full(base_branch).await.unwrap_or_default();
            ctx.agent_context.previous_output = serde_json::json!({
                "diff": truncate(&ciso_diff, 4000),
                "plan": plan_output.data,
                "implementation": impl_output.data,
                "security": security_output.data,
            });

            let ciso_agent = crate::agents::ciso::CisoAgent::new(Arc::clone(&self.router));
            let ciso_output = match ciso_agent.execute(&ctx.agent_context).await {
                Ok(o) => o,
                Err(e) => {
                    warn!(task_id, error = %e, "CISO risk analysis failed");
                    fallback_output(
                        "ciso",
                        serde_json::json!({
                            "risk_verdict": "accept",
                            "risk_score": 0,
                            "blast_radius": "unknown",
                            "aggregate_assessment": "CISO agent failed — defaulting to accept",
                            "trust_boundary_crossings": [],
                            "data_flow_concerns": [],
                            "compliance_flags": [],
                            "operational_risk": {
                                "rollback_complexity": "unknown",
                                "monitoring_gaps": [],
                                "failure_modes": []
                            },
                            "conditions": [],
                            "escalation_reason": null
                        }),
                    )
                }
            };

            let risk_verdict = ciso_output.data["risk_verdict"]
                .as_str()
                .unwrap_or("accept");
            self.emit(&mut ctx, EventKind::CisoReview, ciso_output.data.clone());
            ctx.stage_outputs.insert("ciso".into(), ciso_output.clone());

            if risk_verdict == "escalate" {
                let reason = ciso_output.data["escalation_reason"]
                    .as_str()
                    .unwrap_or("CISO flagged for human review");
                return self
                    .fail(
                        ctx,
                        PipelineStatus::Escalated,
                        format!("CISO escalation: {reason}"),
                    )
                    .await;
            }
        }

        // --- Stage 11: Release assessment (skipped on short circuit) ---
        if use_short_circuit {
            let synthetic = fallback_output(
                "release",
                serde_json::json!({
                    "version_bump": "patch",
                    "reasoning": "skipped (short circuit)"
                }),
            );
            ctx.stage_outputs.insert("release".into(), synthetic);
        } else {
            ctx.current_stage = Some("release".into());
            let diff = workspace.diff_full(base_branch).await.unwrap_or_default();
            ctx.agent_context.previous_output = serde_json::json!({
                "diff": truncate(&diff, 4000),
                "plan": plan_output.data,
            });

            let release_agent = ReleaseAgent::new(Arc::clone(&self.router));
            let release_output = match release_agent.execute(&ctx.agent_context).await {
                Ok(o) => o,
                Err(e) => {
                    warn!(task_id, error = %e, "release assessment failed");
                    fallback_output(
                        "release",
                        serde_json::json!({
                            "version_bump": "patch",
                            "reasoning": "release agent failed"
                        }),
                    )
                }
            };
            self.emit(
                &mut ctx,
                EventKind::ReleaseAssessment,
                release_output.data.clone(),
            );
            ctx.stage_outputs.insert("release".into(), release_output);
        }

        // --- Stage 12: Archive / documentation (skipped on short circuit) ---
        if !use_short_circuit {
            ctx.current_stage = Some("archive".into());
            ctx.agent_context.previous_output = serde_json::json!({
                "plan": plan_output.data,
                "implementation": impl_output.data,
                "security": security_output.data,
            });

            let archivist = ArchivistAgent::new(Arc::clone(&self.router));
            let archive_output = match archivist.execute(&ctx.agent_context).await {
                Ok(o) => o,
                Err(e) => {
                    warn!(task_id, error = %e, "archivist failed");
                    fallback_output(
                        "archivist",
                        serde_json::json!({
                            "adr": null,
                            "doc_updates": [],
                            "architecture_notes": "",
                            "should_write_adr": false
                        }),
                    )
                }
            };
            self.emit(
                &mut ctx,
                EventKind::DocumentationWritten,
                archive_output.data.clone(),
            );
            ctx.stage_outputs.insert("archive".into(), archive_output);
        }

        // --- Stage 12b: Auto-format (best-effort) ---
        // Run `cargo fmt` (Rust) or equivalent before committing so that
        // pre-commit hooks and CI checks don't reject the commit on style.
        if workspace.is_created() {
            let wt = workspace.worktree_path();
            if wt.join("Cargo.toml").exists() {
                let _ = self.ops.cargo_fmt(wt).await;
            }
        }

        // --- Stage 13: Commit ---
        ctx.current_stage = Some("commit".into());
        let commit_msg = impl_output.data["commit_message"]
            .as_str()
            .unwrap_or("chore: automated changes by GLITCHLAB");

        let commit_sha = match workspace.commit(commit_msg).await {
            Ok(Some(sha)) => sha,
            Ok(None) => {
                return self
                    .fail(
                        ctx,
                        PipelineStatus::ImplementationFailed,
                        "no changes produced — implementer output resulted in an empty commit"
                            .into(),
                    )
                    .await;
            }
            Err(e) => {
                return self
                    .fail(ctx, PipelineStatus::Error, format!("commit failed: {e}"))
                    .await;
            }
        };
        self.emit(
            &mut ctx,
            EventKind::Committed,
            serde_json::json!({ "sha": commit_sha }),
        );

        // --- Stage 14: Human gate — PR review (override, before architect) ---
        if self.config.intervention.pause_before_pr {
            let stat = workspace.diff_stat(base_branch).await.unwrap_or_default();
            if !self
                .handler
                .request_approval(
                    "pr_review",
                    &format!("Ready to create PR.\n\n{stat}"),
                    &serde_json::json!({ "diff_stat": stat }),
                )
                .await
            {
                let budget = self.router.budget_summary().await;
                return PipelineResult {
                    status: PipelineStatus::Committed,
                    stage_outputs: ctx.stage_outputs,
                    events: ctx.events,
                    budget,
                    pr_url: None,
                    branch: Some(workspace.branch_name().into()),
                    error: None,
                    outcome_context: None,
                };
            }
        }

        // --- Stage 13b: Architect review ---
        ctx.current_stage = Some("architect_review".into());
        let review_diff = workspace.diff_full(base_branch).await.unwrap_or_default();
        ctx.agent_context.previous_output = serde_json::json!({
            "diff": truncate(&review_diff, 4000),
            "plan": plan_output.data,
            "implementation": impl_output.data,
            "security": security_output.data,
        });

        let review_agent = ArchitectReviewAgent::new(Arc::clone(&self.router));
        let review_output = match review_agent.execute(&ctx.agent_context).await {
            Ok(o) => o,
            Err(e) => {
                warn!(task_id, error = %e, "architect review failed, defaulting to request_changes");
                fallback_output(
                    "architect_review",
                    serde_json::json!({
                        "verdict": "request_changes",
                        "confidence": 0.0,
                        "reasoning": "review agent failed",
                        "quality_score": 0,
                        "issues": [],
                        "architectural_fitness": "unknown",
                        "merge_strategy": "squash"
                    }),
                )
            }
        };

        let review_verdict = review_output.data["verdict"]
            .as_str()
            .unwrap_or("request_changes")
            .to_string();
        self.emit(
            &mut ctx,
            EventKind::ArchitectReview,
            review_output.data.clone(),
        );
        ctx.stage_outputs
            .insert("architect_review".into(), review_output);

        match review_verdict.as_str() {
            "close" => {
                info!(task_id, "architect review: rejected changes");
                return self
                    .fail(
                        ctx,
                        PipelineStatus::ArchitectRejected,
                        "architect review rejected changes".into(),
                    )
                    .await;
            }
            "request_changes" => {
                info!(task_id, "architect review: changes requested");

                let review_data = ctx.stage_outputs.get("architect_review").map(|o| &o.data);
                let issues_summary = review_data
                    .and_then(|d| d.get("issues"))
                    .map(|i| i.to_string())
                    .unwrap_or_default();
                let reasoning = review_data
                    .and_then(|d| d.get("reasoning"))
                    .and_then(|r| r.as_str())
                    .map(String::from);

                let budget = self.router.budget_summary().await;
                return PipelineResult {
                    status: PipelineStatus::Retryable,
                    stage_outputs: ctx.stage_outputs,
                    events: ctx.events,
                    budget,
                    pr_url: None,
                    branch: Some(workspace.branch_name().into()),
                    error: Some(format!("architect requested changes: {issues_summary}")),
                    outcome_context: Some(OutcomeContext {
                        approach: "implementation completed but architect requested changes".into(),
                        obstacle: glitchlab_kernel::outcome::ObstacleKind::ArchitecturalGap {
                            description: issues_summary,
                        },
                        discoveries: vec![],
                        recommendation: reasoning,
                        files_explored: vec![],
                    }),
                };
            }
            _ => {
                // "merge" or any other value — continue to push + PR
            }
        }

        // --- Stage 15: Push + PR ---
        ctx.current_stage = Some("pr".into());
        if let Err(e) = workspace.push().await {
            warn!(task_id, error = %e, "push failed");
            let budget = self.router.budget_summary().await;
            return PipelineResult {
                status: PipelineStatus::Committed,
                stage_outputs: ctx.stage_outputs,
                events: ctx.events,
                budget,
                pr_url: None,
                branch: Some(workspace.branch_name().into()),
                error: Some(format!("push failed: {e}")),
                outcome_context: None,
            };
        }

        let pr_body = build_pr_body(&plan_output, &security_output);
        let pr_title = truncate_head(objective, 70);
        let create_result = self
            .ops
            .create_pr(
                &wt_path,
                workspace.branch_name(),
                base_branch,
                &pr_title,
                &pr_body,
            )
            .await;

        let pr_url = match create_result {
            Ok(url) => url,
            Err(ref e) if e.contains("already exists") => {
                match self
                    .ops
                    .view_existing_pr(&wt_path, workspace.branch_name())
                    .await
                {
                    Ok(url) => {
                        info!(
                            branch = workspace.branch_name(),
                            url = %url,
                            "PR already exists, reusing"
                        );
                        url
                    }
                    Err(_) => {
                        warn!(task_id, error = %e, "PR creation failed");
                        let budget = self.router.budget_summary().await;
                        return PipelineResult {
                            status: PipelineStatus::Committed,
                            stage_outputs: ctx.stage_outputs,
                            events: ctx.events,
                            budget,
                            pr_url: None,
                            branch: Some(workspace.branch_name().into()),
                            error: Some(format!("PR creation failed: {e}")),
                            outcome_context: None,
                        };
                    }
                }
            }
            Err(e) => {
                warn!(task_id, error = %e, "PR creation failed");
                let budget = self.router.budget_summary().await;
                return PipelineResult {
                    status: PipelineStatus::Committed,
                    stage_outputs: ctx.stage_outputs,
                    events: ctx.events,
                    budget,
                    pr_url: None,
                    branch: Some(workspace.branch_name().into()),
                    error: Some(format!("PR creation failed: {e}")),
                    outcome_context: None,
                };
            }
        };

        self.emit(
            &mut ctx,
            EventKind::PrCreated,
            serde_json::json!({ "url": &pr_url }),
        );

        // --- Stage 15b: Post-PR architect review ---
        if self.config.intervention.review_pr_diff {
            ctx.current_stage = Some("architect_pr_review".into());

            // Fetch PR diff from GitHub, fall back to local diff on failure.
            let pr_review_diff = match self.ops.pr_diff(&wt_path, &pr_url).await {
                Ok(d) => d,
                Err(e) => {
                    warn!(task_id, error = %e, "pr_diff failed, falling back to local diff");
                    workspace.diff_full(base_branch).await.unwrap_or_default()
                }
            };

            ctx.agent_context.previous_output = serde_json::json!({
                "review_stage": "post_pr",
                "pr_url": &pr_url,
                "diff": truncate(&pr_review_diff, 4000),
                "plan": plan_output.data,
                "implementation": impl_output.data,
                "security": security_output.data,
            });

            let pr_review_agent = ArchitectReviewAgent::new(Arc::clone(&self.router));
            let pr_review_verdict = match pr_review_agent.execute(&ctx.agent_context).await {
                Ok(o) => {
                    self.emit(&mut ctx, EventKind::ArchitectPrReview, o.data.clone());
                    ctx.stage_outputs
                        .insert("architect_pr_review".into(), o.clone());
                    o.data["verdict"].as_str().unwrap_or("merge").to_string()
                }
                Err(e) => {
                    // Pre-commit review already approved; don't block on agent failure.
                    warn!(task_id, error = %e, "post-PR architect review failed, defaulting to merge");
                    "merge".to_string()
                }
            };

            match pr_review_verdict.as_str() {
                "close" => {
                    info!(task_id, "post-PR architect review: rejected changes");
                    let budget = self.router.budget_summary().await;
                    return PipelineResult {
                        status: PipelineStatus::ArchitectRejected,
                        stage_outputs: ctx.stage_outputs,
                        events: ctx.events,
                        budget,
                        pr_url: Some(pr_url),
                        branch: Some(workspace.branch_name().into()),
                        error: Some("post-PR architect review rejected changes".into()),
                        outcome_context: None,
                    };
                }
                "request_changes" => {
                    info!(task_id, "post-PR architect review: changes requested");

                    let review_data = ctx
                        .stage_outputs
                        .get("architect_pr_review")
                        .map(|o| &o.data);
                    let issues_summary = review_data
                        .and_then(|d| d.get("issues"))
                        .map(|i| i.to_string())
                        .unwrap_or_default();
                    let reasoning = review_data
                        .and_then(|d| d.get("reasoning"))
                        .and_then(|r| r.as_str())
                        .map(String::from);

                    let budget = self.router.budget_summary().await;
                    return PipelineResult {
                        status: PipelineStatus::Retryable,
                        stage_outputs: ctx.stage_outputs,
                        events: ctx.events,
                        budget,
                        pr_url: Some(pr_url),
                        branch: Some(workspace.branch_name().into()),
                        error: Some(format!(
                            "post-PR architect requested changes: {issues_summary}"
                        )),
                        outcome_context: Some(OutcomeContext {
                            approach: "PR created but post-PR architect review requested changes"
                                .into(),
                            obstacle: glitchlab_kernel::outcome::ObstacleKind::ArchitecturalGap {
                                description: issues_summary,
                            },
                            discoveries: vec![],
                            recommendation: reasoning,
                            files_explored: vec![],
                        }),
                    };
                }
                _ => {
                    // "merge" or any other value — proceed to auto-merge.
                }
            }
        }

        // --- Stage 15c: Auto-merge PR ---
        let mut final_status = PipelineStatus::PrCreated;
        match self.ops.merge_pr(&wt_path, &pr_url).await {
            Ok(()) => {
                info!(task_id, "PR auto-merged successfully");
                self.emit(
                    &mut ctx,
                    EventKind::PrMerged,
                    serde_json::json!({ "url": &pr_url }),
                );
                final_status = PipelineStatus::PrMerged;
            }
            Err(e) => {
                warn!(task_id, error = %e, "PR auto-merge failed, PR still open");
            }
        }

        // --- Stage 15d: Close bead ---
        if self.config.memory.beads_enabled {
            let bd_path = self.config.memory.beads_bd_path.as_deref().unwrap_or("bd");
            match self.ops.close_bead(repo_path, bd_path, task_id).await {
                Ok(()) => {
                    info!(task_id, "bead closed successfully");
                }
                Err(e) => {
                    warn!(task_id, error = %e, "bead close failed");
                }
            }
        }

        let budget = self.router.budget_summary().await;
        PipelineResult {
            status: final_status,
            stage_outputs: ctx.stage_outputs,
            events: ctx.events,
            budget,
            pr_url: Some(pr_url),
            branch: Some(workspace.branch_name().into()),
            error: None,
            outcome_context: None,
        }
    }

    fn emit(&self, ctx: &mut PipelineContext, kind: EventKind, data: serde_json::Value) {
        ctx.emit(PipelineEvent {
            kind,
            timestamp: Utc::now().to_rfc3339(),
            task_id: ctx.agent_context.task_id.clone(),
            data,
        });
    }

    async fn fail(
        &self,
        ctx: PipelineContext,
        status: PipelineStatus,
        error: String,
    ) -> PipelineResult {
        self.fail_with_context(ctx, status, error, None).await
    }

    async fn fail_with_context(
        &self,
        ctx: PipelineContext,
        status: PipelineStatus,
        error: String,
        outcome_context: Option<OutcomeContext>,
    ) -> PipelineResult {
        let budget = self.router.budget_summary().await;
        PipelineResult {
            status,
            stage_outputs: ctx.stage_outputs,
            events: ctx.events,
            budget,
            pr_url: None,
            branch: None,
            error: Some(error),
            outcome_context,
        }
    }
}

// ---------------------------------------------------------------------------
// File context loading
// ---------------------------------------------------------------------------

/// Maximum bytes for a single file included in context.
const MAX_FILE_BYTES: usize = 8 * 1024;
/// Maximum total bytes for all files included in context.
const MAX_TOTAL_BYTES: usize = 50 * 1024;

/// Key config files that are always included if they exist in the index.
const CONTEXT_KEY_FILES: &[&str] = &["Cargo.toml", "README.md", "pyproject.toml", "package.json"];

/// Read files relevant to the task from the repo, for injection into agent
/// context. Includes key project files and any files mentioned in the
/// objective text.
async fn read_relevant_files(
    repo_path: &Path,
    objective: &str,
    indexed_files: &[String],
) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let mut total_bytes = 0usize;

    // Collect files to read: key files first, then files mentioned in the objective.
    let mut files_to_read = Vec::new();

    // Always include key config files if they exist in the index.
    for &key_file in CONTEXT_KEY_FILES {
        for indexed in indexed_files {
            // Match both top-level "Cargo.toml" and nested paths like "crates/cli/Cargo.toml"
            // but only include the root-level key file by default.
            if indexed == key_file {
                files_to_read.push(indexed.clone());
            }
        }
    }

    // Scan the objective for file paths that match indexed files.
    for indexed in indexed_files {
        if !files_to_read.contains(indexed) && objective.contains(indexed.as_str()) {
            files_to_read.push(indexed.clone());
        }
    }

    for file in &files_to_read {
        if total_bytes >= MAX_TOTAL_BYTES {
            break;
        }
        let full_path = repo_path.join(file);
        match tokio::fs::read_to_string(&full_path).await {
            Ok(content) => {
                let truncated = if content.len() > MAX_FILE_BYTES {
                    format!("{}...\n[truncated]", &content[..MAX_FILE_BYTES])
                } else {
                    content
                };
                total_bytes += truncated.len();
                result.insert(file.clone(), truncated);
            }
            Err(_) => {
                // File missing or unreadable — skip silently.
            }
        }
    }

    result
}

/// Compute line counts for files already loaded in agent context.
fn compute_file_sizes(file_context: &HashMap<String, String>) -> Vec<(String, usize)> {
    let mut sizes: Vec<(String, usize)> = file_context
        .iter()
        .map(|(path, content)| (path.clone(), content.lines().count()))
        .collect();
    sizes.sort_by_key(|a| std::cmp::Reverse(a.1)); // largest first
    sizes
}

/// Tokens per line of source code.
///
/// Code averages ~32 chars/line, tokenizers average ~4 chars/token → ~8 tokens/line.
/// This is deliberately conservative: overestimating is cheap (an extra decomposition
/// costs ~21K overhead), underestimating is expensive (a budget failure wastes 120K).
const TOKENS_PER_LINE: usize = 8;

/// Format file sizes as a planner constraint string.
fn format_file_size_hint(sizes: &[(String, usize)]) -> String {
    let mut hint = String::from(
        "File sizes (the implementer must read these files into its context window):\n",
    );
    let mut total_lines = 0usize;
    for (path, lines) in sizes {
        let est_tokens = lines * TOKENS_PER_LINE;
        total_lines += lines;
        hint.push_str(&format!("- {path}: {lines} lines (~{est_tokens} tokens)\n"));
    }
    let total_tokens = total_lines * TOKENS_PER_LINE;
    hint.push_str(&format!(
        "Total file context: ~{total_lines} lines (~{total_tokens} tokens/turn, amplified across ~9 TDD turns).\n\
         Implementer budget: ~120K tokens for ~9 turns.\n\
         If total lines exceed 500, decompose so each sub-task scopes to specific \
         line ranges (e.g. 'lines 100-250 of router.rs')."
    ));
    hint
}

/// Estimate the token cost of executing a plan.
///
/// Uses a quadratic model: each turn's input grows as conversation history
/// accumulates. Turn i sends `base_context + i * history_growth` input tokens
/// plus generates `avg_completion` output tokens. The total across N turns is:
///
///   N × (base_context + avg_completion) + history_growth × N × (N-1) / 2
///
/// This properly models the quadratic cost growth that causes budget blowouts.
/// A 1400-line file isn't 11K tokens — it's 11K tokens re-sent every turn
/// PLUS growing conversation history from model responses and tool results.
async fn estimate_plan_tokens(repo_path: &Path, files: &[String], step_count: usize) -> usize {
    let mut file_tokens = 0usize;
    for file in files {
        let full_path = repo_path.join(file);
        if let Ok(content) = tokio::fs::read_to_string(&full_path).await {
            file_tokens += content.lines().count() * TOKENS_PER_LINE;
        }
    }

    // Base context sent as input on every turn:
    // - System prompt (~2K: implementer instructions, tools, workflow)
    // - User message overhead (~3K: objective, constraints, module map)
    // - File content (file_tokens: Relevant File Contents section)
    // With the read cache (gl-eff.3), file_tokens are NOT re-read on
    // subsequent turns, so we only count them once in base_context.
    let base_context = 5_000 + file_tokens;

    // New content added to conversation history each turn:
    // - Model response (~2K: reasoning, tool calls)
    // - Tool results (~2K: edit confirmations, build output)
    // Reduced from 5K: the read cache prevents redundant file content
    // from accumulating in history.
    let history_growth = 4_000;

    // Average completion tokens generated per turn.
    let avg_completion = 2_000;

    // Turns: implementation steps + verify/lint.
    // Sub-tasks (step_count=1) get +2 (implement + verify).
    // Root tasks get +3 (test + implement + verify/lint).
    let turns = if step_count <= 1 {
        step_count + 2
    } else {
        step_count + 3
    };

    // Quadratic total: each turn pays base + accumulated history + completion.
    turns * (base_context + avg_completion) + history_growth * turns * (turns - 1) / 2
}

fn build_pr_body(plan: &AgentOutput, security: &AgentOutput) -> String {
    let mut body = String::from("## Summary\n\n");

    if let Some(steps) = plan.data["steps"].as_array() {
        for step in steps {
            if let Some(desc) = step["description"].as_str() {
                body.push_str(&format!("- {desc}\n"));
            }
        }
    }

    let risk = plan.data["risk_level"].as_str().unwrap_or("unknown");
    body.push_str(&format!("\n**Risk:** {risk}\n"));

    let verdict = security.data["verdict"].as_str().unwrap_or("n/a");
    body.push_str(&format!("**Security:** {verdict}\n"));

    body.push_str("\n---\n_Generated by GLITCHLAB_\n");
    body
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract file-path-like strings from free text (e.g. an objective).
///
/// Splits on whitespace and looks for tokens containing `/` that resemble
/// file paths. Intentionally conservative — false negatives are acceptable
/// (Stage 4 is the real gatekeeper), false positives are cheap (we just
/// run them through the enforcer).
fn extract_file_paths_from_text(text: &str) -> Vec<String> {
    let mut paths: Vec<String> = text
        .split_whitespace()
        .map(|tok| {
            tok.trim_matches(|c: char| {
                c == '`' || c == '\'' || c == '"' || c == '(' || c == ')' || c == ',' || c == ':'
            })
        })
        .filter(|tok| {
            // Must contain at least one `/` and start with a path-like char
            tok.contains('/')
                && tok
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '.' || c == '_')
        })
        .map(|tok| tok.trim_end_matches('/').to_string())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Extract all file paths from a planner output, covering both
/// `files_likely_affected` and every `steps[].files` entry.
/// This prevents the planner from omitting protected paths from the
/// top-level list while still referencing them in step details.
fn extract_all_plan_files(plan: &serde_json::Value) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();

    // 1. files_likely_affected (the primary list)
    if let Some(arr) = plan["files_likely_affected"].as_array() {
        for v in arr {
            if let Some(s) = v.as_str() {
                files.push(s.to_string());
            }
        }
    }

    // 2. steps[].files (per-step file lists)
    if let Some(steps) = plan["steps"].as_array() {
        for step in steps {
            if let Some(step_files) = step["files"].as_array() {
                for v in step_files {
                    if let Some(s) = v.as_str() {
                        files.push(s.to_string());
                    }
                }
            }
        }
    }

    // 3. decomposition[].objective — extract paths from sub-task descriptions
    if let Some(subs) = plan["decomposition"].as_array() {
        for sub in subs {
            if let Some(obj) = sub["objective"].as_str() {
                files.extend(extract_file_paths_from_text(obj));
            }
        }
    }

    files.sort();
    files.dedup();
    files
}

/// Determine whether the planner output qualifies for short-circuit mode.
///
/// Uses structural signals (file count, step count, decomposition) as the
/// primary check rather than trusting the LLM's stated complexity label.
/// A task is eligible when it has no decomposition, no core change, few
/// files, few steps, and complexity is not "large".
fn is_short_circuit_eligible(plan_data: &serde_json::Value, max_files: usize) -> bool {
    // Decomposed tasks need the full pipeline.
    if plan_data
        .get("decomposition")
        .is_some_and(|d| d.is_array() && !d.as_array().unwrap().is_empty())
    {
        tracing::debug!("short-circuit: ineligible — task has decomposition");
        return false;
    }

    // Core changes need full review. Default false — boundary checking
    // is enforced independently by the tool dispatcher.
    if plan_data["requires_core_change"].as_bool().unwrap_or(false) {
        tracing::debug!("short-circuit: ineligible — requires core change");
        return false;
    }

    let file_count = plan_data["files_likely_affected"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    if file_count > max_files || file_count == 0 {
        tracing::debug!(
            file_count,
            max_files,
            "short-circuit: ineligible — file count out of range"
        );
        return false;
    }

    // More than 3 steps is structurally too complex for short-circuit.
    let step_count = plan_data["steps"].as_array().map(|a| a.len()).unwrap_or(0);
    if step_count > 3 {
        tracing::debug!(step_count, "short-circuit: ineligible — too many steps");
        return false;
    }

    // Reject only "large" — trust structural signals over the LLM's label.
    // Sub-tasks from decomposition are structurally small but often labelled
    // "medium" by the planner; rejecting "medium" would defeat the purpose.
    let complexity = plan_data["estimated_complexity"]
        .as_str()
        .unwrap_or("medium");
    if complexity == "large" {
        tracing::debug!(complexity, "short-circuit: ineligible — large complexity");
        return false;
    }

    tracing::debug!(
        file_count,
        step_count,
        complexity,
        "short-circuit: eligible"
    );
    true
}

/// Build a lightweight Rust module map for the given file paths.
///
/// Walks up parent directories looking for `lib.rs` and `mod.rs` files,
/// extracting only `pub mod`, `mod`, `pub use`, and `pub(crate) mod`
/// declarations. Returns a markdown-formatted string suitable for injection
/// into the agent context.
fn build_rust_module_map(worktree: &Path, files: &[String]) -> String {
    use std::collections::BTreeSet;

    let mut module_files: BTreeSet<PathBuf> = BTreeSet::new();

    for file in files {
        let file_path = Path::new(file);
        // Walk up parent directories looking for module root files.
        let mut dir = file_path.parent();
        while let Some(d) = dir {
            if d.as_os_str().is_empty() {
                break;
            }
            module_files.insert(d.join("lib.rs"));
            module_files.insert(d.join("mod.rs"));
            dir = d.parent();
        }
    }

    let mut sections = Vec::new();
    for mod_file in &module_files {
        let full_path = worktree.join(mod_file);
        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let mod_lines: Vec<&str> = content
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                trimmed.starts_with("pub mod ")
                    || trimmed.starts_with("mod ")
                    || trimmed.starts_with("pub use ")
                    || trimmed.starts_with("pub(crate) mod ")
            })
            .collect();

        if mod_lines.is_empty() {
            continue;
        }

        let mut section = format!("### `{}`\n", mod_file.display());
        for line in mod_lines {
            section.push_str(&format!("- `{}`\n", line.trim()));
        }
        sections.push(section);
    }

    if sections.is_empty() {
        return String::new();
    }

    let mut result = String::from("## Rust Module Map\n\n");
    result.push_str(&sections.join("\n"));
    result
}

fn format_plan_summary(plan: &AgentOutput) -> String {
    let mut summary = String::from("## Plan Summary\n\n");

    if let Some(steps) = plan.data["steps"].as_array() {
        for step in steps {
            let num = step["step_number"].as_u64().unwrap_or(0);
            let desc = step["description"].as_str().unwrap_or("?");
            summary.push_str(&format!("{num}. {desc}\n"));
        }
    }

    let risk = plan.data["risk_level"].as_str().unwrap_or("unknown");
    summary.push_str(&format!("\nRisk: {risk}\n"));

    if let Some(files) = plan.data["files_likely_affected"].as_array() {
        summary.push_str("\nFiles affected:\n");
        for f in files {
            if let Some(s) = f.as_str() {
                summary.push_str(&format!("- {s}\n"));
            }
        }
    }

    summary
}

/// Truncate keeping the **tail** (last `max` chars). Command output and diffs
/// have the interesting bits (errors, summaries) at the end.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let tail = &s[s.len() - max..];
        // Try to break at a newline so we don't start mid-line.
        if let Some(nl) = tail.find('\n') {
            format!("[...truncated]\n{}", &tail[nl + 1..])
        } else {
            format!("[...truncated]\n{tail}")
        }
    }
}

/// Truncate keeping the **head** (first `max` chars). Used for titles / labels.
fn truncate_head(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

fn fallback_output(agent: &str, data: serde_json::Value) -> AgentOutput {
    AgentOutput {
        data,
        metadata: AgentMetadata {
            agent: agent.into(),
            model: "none".into(),
            tokens: 0,
            cost: 0.0,
            latency_ms: 0,
        },
        parse_error: true,
    }
}

fn build_history_entry(task_id: &str, result: &PipelineResult) -> HistoryEntry {
    let status = serde_json::to_value(result.status)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| format!("{:?}", result.status));

    HistoryEntry {
        timestamp: Utc::now(),
        task_id: task_id.into(),
        status,
        pr_url: result.pr_url.clone(),
        branch: result.branch.clone(),
        error: result.error.clone(),
        budget: BudgetSummary {
            total_tokens: result.budget.total_tokens,
            estimated_cost: result.budget.estimated_cost,
            call_count: result.budget.call_count,
            tokens_remaining: result.budget.tokens_remaining,
            dollars_remaining: result.budget.dollars_remaining,
        },
        events_summary: build_events_summary(&result.stage_outputs),
        stage_outputs: None,
        events: None,
        outcome_context: result.outcome_context.clone(),
    }
}

fn build_events_summary(stage_outputs: &HashMap<String, AgentOutput>) -> EventsSummary {
    let plan_steps = stage_outputs
        .get("plan")
        .and_then(|o| o.data["steps"].as_array())
        .map(|a| a.len() as u32)
        .unwrap_or(0);

    let plan_risk = stage_outputs
        .get("plan")
        .and_then(|o| o.data["risk_level"].as_str())
        .unwrap_or("")
        .to_string();

    let security_verdict = stage_outputs
        .get("security")
        .and_then(|o| o.data["verdict"].as_str())
        .unwrap_or("")
        .to_string();

    let version_bump = stage_outputs
        .get("release")
        .and_then(|o| o.data["version_bump"].as_str())
        .unwrap_or("")
        .to_string();

    let fix_attempts = stage_outputs
        .keys()
        .filter(|k| k.starts_with("debug_"))
        .count() as u32;

    let tests_passed_on_attempt = if stage_outputs.contains_key("implement") {
        fix_attempts + 1
    } else {
        0
    };

    EventsSummary {
        plan_steps,
        plan_risk,
        tests_passed_on_attempt,
        security_verdict,
        version_bump,
        fix_attempts,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::test_helpers::{
        final_response, mock_router_ref, sequential_router_ref, tool_response,
    };
    use glitchlab_kernel::agent::{AgentMetadata, AgentOutput};
    use glitchlab_kernel::budget::BudgetSummary;
    use glitchlab_kernel::tool::ToolCall;
    use glitchlab_memory::history::JsonlHistory;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // -----------------------------------------------------------------------
    // MockExternalOps — test double for ExternalOps
    // -----------------------------------------------------------------------

    /// Configurable mock for [`ExternalOps`]. Each operation has a queue of
    /// results; when the queue is empty the default (success) is returned.
    struct MockExternalOps {
        fmt_results: Mutex<VecDeque<Result<(), String>>>,
        test_results: Mutex<VecDeque<Result<(), String>>>,
        create_pr_results: Mutex<VecDeque<Result<String, String>>>,
        view_pr_results: Mutex<VecDeque<Result<String, String>>>,
        pr_diff_results: Mutex<VecDeque<Result<String, String>>>,
        merge_results: Mutex<VecDeque<Result<(), String>>>,
        close_bead_results: Mutex<VecDeque<Result<(), String>>>,
    }

    impl Default for MockExternalOps {
        fn default() -> Self {
            Self {
                fmt_results: Mutex::new(VecDeque::new()),
                test_results: Mutex::new(VecDeque::new()),
                create_pr_results: Mutex::new(VecDeque::new()),
                view_pr_results: Mutex::new(VecDeque::new()),
                pr_diff_results: Mutex::new(VecDeque::new()),
                merge_results: Mutex::new(VecDeque::new()),
                close_bead_results: Mutex::new(VecDeque::new()),
            }
        }
    }

    impl ExternalOps for MockExternalOps {
        fn cargo_fmt<'a>(
            &'a self,
            _worktree: &'a Path,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async move {
                self.fmt_results
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(Ok(()))
            })
        }

        fn run_tests<'a>(
            &'a self,
            _cmd: &'a str,
            _worktree: &'a Path,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async move {
                self.test_results
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(Ok(()))
            })
        }

        fn create_pr<'a>(
            &'a self,
            _worktree: &'a Path,
            _branch: &'a str,
            _base: &'a str,
            _title: &'a str,
            _body: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                self.create_pr_results
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Ok("https://github.com/test/repo/pull/1".into()))
            })
        }

        fn view_existing_pr<'a>(
            &'a self,
            _worktree: &'a Path,
            _branch: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                self.view_pr_results
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Ok("https://github.com/test/repo/pull/1".into()))
            })
        }

        fn pr_diff<'a>(
            &'a self,
            _worktree: &'a Path,
            _pr_url: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                self.pr_diff_results
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Ok("diff --git a/file.rs b/file.rs\n+added line\n".into()))
            })
        }

        fn merge_pr<'a>(
            &'a self,
            _worktree: &'a Path,
            _pr_url: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async move {
                self.merge_results
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(Ok(()))
            })
        }

        fn close_bead<'a>(
            &'a self,
            _repo_path: &'a Path,
            _bd_path: &'a str,
            _task_id: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async move {
                self.close_bead_results
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(Ok(()))
            })
        }
    }

    fn default_mock_ops() -> Arc<dyn ExternalOps> {
        Arc::new(MockExternalOps::default())
    }

    /// Build the scripted sequence of LLM responses for a full pipeline run.
    ///
    /// Order: planner → implementer (tool_use) → implementer (final) →
    ///        security → release → archivist.
    fn pipeline_mock_responses() -> Vec<glitchlab_router::RouterResponse> {
        vec![
            // 1. Planner
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature", "files": ["src/new.rs"], "action": "create"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "trivial", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed
            final_response(
                r#"{"verdict": "proceed", "confidence": 0.9, "reasoning": "work not yet done", "evidence": [], "architectural_notes": "", "suggested_changes": []}"#,
            ),
            // 3. Implementer — write_file tool call
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/new.rs", "content": "pub fn greet() -> &'static str { \"hello\" }\n"}),
            }]),
            // 4. Implementer — final metadata
            final_response(
                r#"{"files_changed": ["src/new.rs"], "tests_added": [], "commit_message": "feat: add greet function", "summary": "test"}"#,
            ),
            // 5. Security
            final_response(r#"{"verdict": "pass", "issues": [], "summary": "no issues"}"#),
            // 6. CISO
            final_response(
                r#"{"risk_verdict": "accept", "risk_score": 2, "blast_radius": "isolated", "trust_boundary_crossings": [], "data_flow_concerns": [], "compliance_flags": [], "operational_risk": {"rollback_complexity": "trivial", "monitoring_gaps": [], "failure_modes": []}, "aggregate_assessment": "low risk", "conditions": [], "escalation_reason": null}"#,
            ),
            // 7. Release
            final_response(
                r#"{"version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": []}"#,
            ),
            // 8. Archivist
            final_response(
                r#"{"adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#,
            ),
            // 9. Architect review — merge
            final_response(
                r#"{"verdict": "merge", "confidence": 0.9, "reasoning": "looks good", "quality_score": 8, "issues": [], "architectural_fitness": "good", "merge_strategy": "squash"}"#,
            ),
        ]
    }

    #[tokio::test]
    async fn auto_approve_handler() {
        let handler = AutoApproveHandler;
        let result = handler
            .request_approval("test", "summary", &serde_json::Value::Null)
            .await;
        assert!(result);
    }

    #[test]
    fn pipeline_construction() {
        let dir = tempfile::tempdir().unwrap();
        let router = mock_router_ref();
        let config = EngConfig::default();
        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let _pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());
    }

    #[tokio::test]
    async fn real_ops_run_tests_success() {
        let dir = tempfile::tempdir().unwrap();
        let ops = RealExternalOps;
        // `true` always succeeds on Unix.
        ops.run_tests("true", dir.path()).await.unwrap();
    }

    #[tokio::test]
    async fn real_ops_run_tests_failure() {
        let dir = tempfile::tempdir().unwrap();
        let ops = RealExternalOps;
        // `false` always fails on Unix.
        let result = ops.run_tests("false", dir.path()).await;
        assert!(result.is_err());
    }

    #[test]
    fn format_plan_summary_with_steps() {
        let plan = AgentOutput {
            data: serde_json::json!({
                "steps": [
                    {"step_number": 1, "description": "Add module"},
                    {"step_number": 2, "description": "Write tests"},
                ],
                "risk_level": "low",
                "files_likely_affected": ["src/lib.rs"],
            }),
            metadata: AgentMetadata {
                agent: "planner".into(),
                model: "test".into(),
                tokens: 0,
                cost: 0.0,
                latency_ms: 0,
            },
            parse_error: false,
        };

        let summary = format_plan_summary(&plan);
        assert!(summary.contains("1. Add module"));
        assert!(summary.contains("2. Write tests"));
        assert!(summary.contains("Risk: low"));
        assert!(summary.contains("src/lib.rs"));
    }

    #[test]
    fn format_plan_summary_empty() {
        let plan = AgentOutput {
            data: serde_json::json!({}),
            metadata: AgentMetadata {
                agent: "planner".into(),
                model: "test".into(),
                tokens: 0,
                cost: 0.0,
                latency_ms: 0,
            },
            parse_error: false,
        };

        let summary = format_plan_summary(&plan);
        assert!(summary.contains("Plan Summary"));
        assert!(summary.contains("Risk: unknown"));
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_keeps_tail() {
        let input = "line1\nline2\nline3\nerror: something broke";
        let result = truncate(input, 25);
        // Should keep the tail and break at a newline boundary.
        assert!(
            result.starts_with("[...truncated]\n"),
            "should start with truncation marker, got: {result}"
        );
        assert!(
            result.contains("error: something broke"),
            "should contain the tail error, got: {result}"
        );
        assert!(
            !result.contains("line1"),
            "should not contain the head, got: {result}"
        );
    }

    #[test]
    fn truncate_no_newline_in_tail() {
        // When tail has no newline, just prefix with marker.
        let result = truncate("abcdefghij", 5);
        assert_eq!(result, "[...truncated]\nfghij");
    }

    #[test]
    fn truncate_head_short_string() {
        assert_eq!(truncate_head("hello", 10), "hello");
    }

    #[test]
    fn truncate_head_long_string() {
        let result = truncate_head("hello world", 5);
        assert_eq!(result, "hello...");
    }

    #[test]
    fn fallback_output_construction() {
        let output = fallback_output("test-agent", serde_json::json!({"key": "value"}));
        assert_eq!(output.metadata.agent, "test-agent");
        assert_eq!(output.metadata.model, "none");
        assert!(output.parse_error);
        assert_eq!(output.data["key"], "value");
    }

    #[test]
    fn build_pr_body_with_steps() {
        let plan = AgentOutput {
            data: serde_json::json!({
                "steps": [
                    {"description": "Fix the bug"},
                    {"description": "Add tests"},
                ],
                "risk_level": "medium",
            }),
            metadata: AgentMetadata {
                agent: "planner".into(),
                model: "test".into(),
                tokens: 0,
                cost: 0.0,
                latency_ms: 0,
            },
            parse_error: false,
        };
        let security = AgentOutput {
            data: serde_json::json!({"verdict": "pass"}),
            metadata: AgentMetadata {
                agent: "security".into(),
                model: "test".into(),
                tokens: 0,
                cost: 0.0,
                latency_ms: 0,
            },
            parse_error: false,
        };

        let body = build_pr_body(&plan, &security);
        assert!(body.contains("Fix the bug"));
        assert!(body.contains("Add tests"));
        assert!(body.contains("**Risk:** medium"));
        assert!(body.contains("**Security:** pass"));
        assert!(body.contains("GLITCHLAB"));
    }

    #[test]
    fn build_history_entry_from_result() {
        let result = PipelineResult {
            status: PipelineStatus::PrCreated,
            stage_outputs: HashMap::from([(
                "plan".into(),
                AgentOutput {
                    data: serde_json::json!({
                        "steps": [{"step_number": 1, "description": "step"}],
                        "risk_level": "low",
                    }),
                    metadata: AgentMetadata {
                        agent: "planner".into(),
                        model: "test".into(),
                        tokens: 100,
                        cost: 0.01,
                        latency_ms: 50,
                    },
                    parse_error: false,
                },
            )]),
            events: vec![],
            budget: BudgetSummary {
                total_tokens: 500,
                estimated_cost: 0.05,
                call_count: 3,
                tokens_remaining: 99_500,
                dollars_remaining: 9.95,
            },
            pr_url: Some("https://github.com/test/repo/pull/1".into()),
            branch: Some("glitchlab/test-1".into()),
            error: None,
            outcome_context: None,
        };

        let entry = build_history_entry("test-1", &result);
        assert_eq!(entry.task_id, "test-1");
        assert_eq!(entry.status, "pr_created");
        assert!(entry.pr_url.is_some());
        assert!(entry.branch.is_some());
        assert!(entry.error.is_none());
        assert_eq!(entry.budget.total_tokens, 500);
        assert_eq!(entry.budget.call_count, 3);
        assert!(entry.outcome_context.is_none());
    }

    #[test]
    fn build_history_entry_with_outcome_context() {
        use glitchlab_kernel::outcome::{ObstacleKind, OutcomeContext};

        let result = PipelineResult {
            status: PipelineStatus::ImplementationFailed,
            stage_outputs: HashMap::new(),
            events: vec![],
            budget: BudgetSummary {
                total_tokens: 200,
                estimated_cost: 0.02,
                call_count: 1,
                tokens_remaining: 99_800,
                dollars_remaining: 9.98,
            },
            pr_url: None,
            branch: None,
            error: Some("implementer failed".into()),
            outcome_context: Some(OutcomeContext {
                approach: "tried adding function".into(),
                obstacle: ObstacleKind::ModelLimitation {
                    model: "test-model".into(),
                    error_class: "parse_error".into(),
                },
                discoveries: vec!["file uses macros".into()],
                recommendation: None,
                files_explored: vec!["src/lib.rs".into()],
            }),
        };

        let entry = build_history_entry("fail-ctx", &result);
        assert_eq!(entry.status, "implementation_failed");
        let oc = entry.outcome_context.unwrap();
        assert_eq!(oc.approach, "tried adding function");
        assert_eq!(oc.discoveries.len(), 1);
    }

    #[test]
    fn build_events_summary_with_data() {
        let outputs = HashMap::from([
            (
                "plan".into(),
                AgentOutput {
                    data: serde_json::json!({
                        "steps": [{"n": 1}, {"n": 2}],
                        "risk_level": "medium",
                    }),
                    metadata: AgentMetadata {
                        agent: "planner".into(),
                        model: "test".into(),
                        tokens: 0,
                        cost: 0.0,
                        latency_ms: 0,
                    },
                    parse_error: false,
                },
            ),
            (
                "implement".into(),
                AgentOutput {
                    data: serde_json::json!({}),
                    metadata: AgentMetadata {
                        agent: "implementer".into(),
                        model: "test".into(),
                        tokens: 0,
                        cost: 0.0,
                        latency_ms: 0,
                    },
                    parse_error: false,
                },
            ),
            (
                "security".into(),
                AgentOutput {
                    data: serde_json::json!({"verdict": "pass"}),
                    metadata: AgentMetadata {
                        agent: "security".into(),
                        model: "test".into(),
                        tokens: 0,
                        cost: 0.0,
                        latency_ms: 0,
                    },
                    parse_error: false,
                },
            ),
            (
                "release".into(),
                AgentOutput {
                    data: serde_json::json!({"version_bump": "minor"}),
                    metadata: AgentMetadata {
                        agent: "release".into(),
                        model: "test".into(),
                        tokens: 0,
                        cost: 0.0,
                        latency_ms: 0,
                    },
                    parse_error: false,
                },
            ),
            (
                "debug_1".into(),
                AgentOutput {
                    data: serde_json::json!({}),
                    metadata: AgentMetadata {
                        agent: "debugger".into(),
                        model: "test".into(),
                        tokens: 0,
                        cost: 0.0,
                        latency_ms: 0,
                    },
                    parse_error: false,
                },
            ),
        ]);

        let summary = build_events_summary(&outputs);
        assert_eq!(summary.plan_steps, 2);
        assert_eq!(summary.plan_risk, "medium");
        assert_eq!(summary.security_verdict, "pass");
        assert_eq!(summary.version_bump, "minor");
        assert_eq!(summary.fix_attempts, 1);
        assert_eq!(summary.tests_passed_on_attempt, 2);
    }

    #[test]
    fn build_events_summary_empty() {
        let summary = build_events_summary(&HashMap::new());
        assert_eq!(summary.plan_steps, 0);
        assert_eq!(summary.plan_risk, "");
        assert_eq!(summary.fix_attempts, 0);
        assert_eq!(summary.tests_passed_on_attempt, 0);
    }

    #[tokio::test]
    async fn pipeline_with_previous_attempts_injects_context() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let prev = vec![glitchlab_kernel::outcome::OutcomeContext {
            approach: "tried direct edit".into(),
            obstacle: glitchlab_kernel::outcome::ObstacleKind::TestFailure {
                attempts: 2,
                last_error: "assertion failed".into(),
            },
            discoveries: vec!["module uses trait objects".into()],
            recommendation: Some("mock the trait".into()),
            files_explored: vec!["src/lib.rs".into()],
        }];

        let result = pipeline
            .run(
                "test-with-ctx",
                "Fix a bug",
                dir.path(),
                &base_branch,
                &prev,
            )
            .await;

        // The pipeline should still complete (previous_attempts is informational).
        assert!(
            result.status == PipelineStatus::Committed
                || result.status == PipelineStatus::PrCreated,
            "unexpected status: {:?}, error: {:?}",
            result.status,
            result.error
        );
    }

    #[tokio::test]
    async fn pipeline_integration_with_git_repo() {
        let dir = tempfile::tempdir().unwrap();

        // Initialize a git repo with a commit.
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("README.md"), "# Test").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let output = std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let base_branch = String::from_utf8_lossy(&output.stdout).trim().to_string();

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run("test-task-1", "Fix a bug", dir.path(), &base_branch, &[])
            .await;

        // Pipeline runs through all stages. Push fails (no remote),
        // so final status is Committed.
        assert!(
            result.status == PipelineStatus::Committed
                || result.status == PipelineStatus::PrCreated,
            "unexpected status: {:?}, error: {:?}",
            result.status,
            result.error
        );
        assert!(!result.events.is_empty());
        assert!(result.stage_outputs.contains_key("plan"));
        assert!(result.stage_outputs.contains_key("implement"));
        assert!(result.stage_outputs.contains_key("security"));
        assert!(result.stage_outputs.contains_key("release"));
        assert!(result.stage_outputs.contains_key("archive"));
    }

    struct RejectHandler;

    impl InterventionHandler for RejectHandler {
        fn request_approval(
            &self,
            _gate: &str,
            _summary: &str,
            _data: &serde_json::Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            Box::pin(async { false })
        }
    }

    #[tokio::test]
    async fn pipeline_plan_rejected() {
        let dir = tempfile::tempdir().unwrap();

        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("README.md"), "# Test").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let output = std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let base_branch = String::from_utf8_lossy(&output.stdout).trim().to_string();

        let router = mock_router_ref();
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = true;

        let handler = Arc::new(RejectHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-reject",
                "Fix something",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::Interrupted);
        assert!(result.error.as_deref().unwrap().contains("rejected"));
    }

    /// Helper: initialize a git repo with a commit, return the base branch name.
    fn init_test_repo(dir: &Path) -> String {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::fs::write(dir.join("README.md"), "# Test").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(dir)
            .output()
            .unwrap();

        let output = std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn real_ops_run_tests_empty_command() {
        let dir = tempfile::tempdir().unwrap();
        let ops = RealExternalOps;
        ops.run_tests("", dir.path()).await.unwrap();
    }

    #[tokio::test]
    async fn pipeline_with_test_command() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // Add a Makefile with a test target that succeeds.
        std::fs::write(dir.path().join("Makefile"), "test:\n\t@echo tests pass\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "Makefile"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add Makefile"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-with-tests",
                "Fix a bug",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // Tests should pass, then push fails (no remote) → Committed.
        assert!(
            result.status == PipelineStatus::Committed
                || result.status == PipelineStatus::PrCreated,
            "unexpected status: {:?}, error: {:?}",
            result.status,
            result.error
        );
        // Check that TestsPassed event was emitted.
        assert!(
            result
                .events
                .iter()
                .any(|e| e.kind == EventKind::TestsPassed),
            "expected TestsPassed event"
        );
    }

    /// Handler that approves plan review but rejects PR review.
    struct PrRejectHandler;

    impl InterventionHandler for PrRejectHandler {
        fn request_approval(
            &self,
            gate: &str,
            _summary: &str,
            _data: &serde_json::Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            let approve = gate != "pr_review";
            Box::pin(async move { approve })
        }
    }

    #[tokio::test]
    async fn pipeline_pr_review_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = true;
        config.intervention.pause_before_pr = true;

        let handler = Arc::new(PrRejectHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-pr-reject",
                "Fix something",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // Pipeline should commit but stop before PR.
        assert_eq!(result.status, PipelineStatus::Committed);
        assert!(result.error.is_none());
        assert!(result.branch.is_some());
        assert!(result.pr_url.is_none());
    }

    #[tokio::test]
    async fn pipeline_tests_fail_debug_no_retry() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // Makefile with test target that always fails.
        std::fs::write(
            dir.path().join("Makefile"),
            "test:\n\t@echo FAIL && exit 1\n",
        )
        .unwrap();
        std::process::Command::new("git")
            .args(["add", "Makefile"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add failing test"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let responses = vec![
            // 1. Planner
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "fix bug"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed
            final_response(
                r#"{"verdict": "proceed", "confidence": 0.9, "reasoning": "not done", "evidence": [], "architectural_notes": "", "suggested_changes": []}"#,
            ),
            // 3. Implementer — write_file tool call
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/new.rs", "content": "pub fn greet() -> &'static str { \"hello\" }\n"}),
            }]),
            // 4. Implementer — final metadata
            final_response(
                r#"{"files_changed": ["src/new.rs"], "tests_added": [], "commit_message": "fix: bug", "summary": "fixed"}"#,
            ),
            // 5. Debugger — should_retry: false
            final_response(
                r#"{"diagnosis": "test failure", "root_cause": "bug", "files_changed": [], "confidence": "low", "should_retry": false, "notes": null}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let mock_ops = MockExternalOps::default();
        mock_ops
            .test_results
            .lock()
            .unwrap()
            .push_back(Err("FAIL\n".into()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run(
                "test-fail-debug",
                "Fix a bug",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // Mock debugger returns should_retry: false, so pipeline stops.
        assert_eq!(result.status, PipelineStatus::TestsFailed);
        assert!(result.error.is_some());
        // Should have TestsFailed and DebugAttempt events.
        assert!(
            result
                .events
                .iter()
                .any(|e| e.kind == EventKind::TestsFailed)
        );
        assert!(
            result
                .events
                .iter()
                .any(|e| e.kind == EventKind::DebugAttempt)
        );
        // Should have a debug_1 stage output.
        assert!(result.stage_outputs.contains_key("debug_1"));
    }

    #[tokio::test]
    async fn real_ops_create_pr_without_gh() {
        let dir = tempfile::tempdir().unwrap();
        let ops = RealExternalOps;
        // gh pr create will fail — no git repo, no remote.
        let result = ops
            .create_pr(dir.path(), "test-branch", "main", "Test PR", "body")
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn real_ops_view_existing_pr_no_repo() {
        let dir = tempfile::tempdir().unwrap();
        let ops = RealExternalOps;
        // gh pr view will fail — no git repo, no remote.
        let result = ops.view_existing_pr(dir.path(), "test-branch").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn real_ops_run_tests_multiword_command() {
        let dir = tempfile::tempdir().unwrap();
        let ops = RealExternalOps;
        // Multi-word command that succeeds.
        ops.run_tests("echo hello world", dir.path()).await.unwrap();
    }

    // --- read_relevant_files tests ---

    #[tokio::test]
    async fn read_relevant_files_includes_key_files() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"test\"")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("README.md"), "# Hello")
            .await
            .unwrap();

        let indexed = vec!["Cargo.toml".to_string(), "README.md".to_string()];
        let result = read_relevant_files(dir.path(), "some objective", &indexed).await;

        assert!(result.contains_key("Cargo.toml"));
        assert!(result.contains_key("README.md"));
    }

    #[tokio::test]
    async fn read_relevant_files_picks_up_mentioned_files() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join("src"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("src/lib.rs"), "pub fn hello() {}")
            .await
            .unwrap();

        let indexed = vec!["src/lib.rs".to_string()];
        let result =
            read_relevant_files(dir.path(), "Fix the bug in src/lib.rs please", &indexed).await;

        assert!(result.contains_key("src/lib.rs"));
        assert!(result["src/lib.rs"].contains("pub fn hello()"));
    }

    #[tokio::test]
    async fn read_relevant_files_respects_total_limit() {
        let dir = tempfile::tempdir().unwrap();
        // Create files that together exceed MAX_TOTAL_BYTES (50KB).
        let big_content = "x".repeat(MAX_FILE_BYTES); // 8KB each
        let mut indexed = Vec::new();
        for i in 0..10 {
            let name = format!("file_{i}.rs");
            tokio::fs::write(dir.path().join(&name), &big_content)
                .await
                .unwrap();
            indexed.push(name.clone());
        }

        // Mention all files in objective so they're candidates.
        let objective = indexed.join(" ");
        let result = read_relevant_files(dir.path(), &objective, &indexed).await;

        let total: usize = result.values().map(|v| v.len()).sum();
        assert!(
            total <= MAX_TOTAL_BYTES + MAX_FILE_BYTES,
            "total {total} should be bounded"
        );
    }

    #[tokio::test]
    async fn read_relevant_files_handles_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let indexed = vec!["nonexistent.rs".to_string()];
        let result = read_relevant_files(dir.path(), "Fix nonexistent.rs", &indexed).await;
        assert!(result.is_empty());
    }

    // --- compute_file_sizes / format_file_size_hint / estimate_plan_tokens tests ---

    #[test]
    fn compute_file_sizes_returns_sorted_by_largest() {
        let mut ctx = HashMap::new();
        ctx.insert("small.rs".into(), "line1\nline2\n".into());
        ctx.insert("big.rs".into(), "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n".into());

        let sizes = compute_file_sizes(&ctx);
        assert_eq!(sizes.len(), 2);
        // Largest first.
        assert_eq!(sizes[0].0, "big.rs");
        assert_eq!(sizes[0].1, 10);
        assert_eq!(sizes[1].0, "small.rs");
        assert_eq!(sizes[1].1, 2);
    }

    #[test]
    fn format_file_size_hint_output() {
        let sizes = vec![
            ("src/router.rs".into(), 1400usize),
            ("Cargo.toml".into(), 30usize),
        ];
        let hint = format_file_size_hint(&sizes);
        assert!(
            hint.contains("src/router.rs: 1400 lines (~11200 tokens)"),
            "should list router.rs with 8 tokens/line: {hint}"
        );
        assert!(
            hint.contains("Cargo.toml: 30 lines (~240 tokens)"),
            "should list Cargo.toml: {hint}"
        );
        assert!(
            hint.contains("Total file context: ~1430 lines"),
            "should sum lines: {hint}"
        );
        assert!(
            hint.contains("Implementer budget: ~120K tokens"),
            "should mention budget: {hint}"
        );
        assert!(
            hint.contains("If total lines exceed 500"),
            "should mention decomposition threshold: {hint}"
        );
    }

    #[tokio::test]
    async fn estimate_plan_tokens_basic() {
        let dir = tempfile::tempdir().unwrap();
        // 100-line file → 800 file tokens (8 tokens/line)
        let content: String = (0..100).map(|i| format!("line {i}\n")).collect();
        tokio::fs::create_dir_all(dir.path().join("src"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("src/lib.rs"), &content)
            .await
            .unwrap();

        let files = vec!["src/lib.rs".to_string()];
        let estimated = estimate_plan_tokens(dir.path(), &files, 3).await;

        // Quadratic model: turns = step_count + 3 = 6 (root task, step_count=3)
        // base_context = 5K + 800 = 5,800
        // total = 6 * (5800 + 2000) + 4000 * 6 * 5 / 2
        //       = 46,800 + 60,000 = 106,800
        let file_tokens = 100 * TOKENS_PER_LINE;
        let base_context = 5_000 + file_tokens;
        let turns = 6;
        let expected = turns * (base_context + 2_000) + 4_000 * turns * (turns - 1) / 2;
        assert_eq!(
            estimated, expected,
            "should calculate correct token estimate"
        );
    }

    #[tokio::test]
    async fn estimate_plan_tokens_missing_files() {
        let dir = tempfile::tempdir().unwrap();

        let files = vec!["nonexistent.rs".to_string()];
        let estimated = estimate_plan_tokens(dir.path(), &files, 2).await;

        // Missing files contribute 0 file tokens.
        // turns = 2 + 3 = 5 (root task, step_count=2), base_context = 5K
        // total = 5 * (5K + 2K) + 4K * 5 * 4 / 2 = 35K + 40K = 75K
        let turns = 5;
        let expected = turns * (5_000 + 2_000) + 4_000 * turns * (turns - 1) / 2;
        assert_eq!(
            estimated, expected,
            "missing files should contribute 0 tokens"
        );
    }

    #[test]
    fn implementer_efficiency_default() {
        let eff = ImplementerEfficiency::default();
        assert_eq!(eff.total_tool_calls, 0);
        assert_eq!(eff.redundant_reads, 0);
        assert_eq!(eff.list_files_calls, 0);
    }

    #[test]
    fn implementer_efficiency_serializable() {
        let eff = ImplementerEfficiency {
            total_tool_calls: 10,
            redundant_reads: 3,
            list_files_calls: 1,
        };
        let json = serde_json::to_value(&eff).unwrap();
        assert_eq!(json["redundant_reads"], 3);
        let deser: ImplementerEfficiency = serde_json::from_value(json).unwrap();
        assert_eq!(deser.total_tool_calls, 10);
    }

    #[tokio::test]
    async fn estimate_plan_tokens_catches_known_failure() {
        // Calibration: the canary failure was a 1405-line file that blew
        // 124K/120K budget. The estimator MUST flag this.
        let dir = tempfile::tempdir().unwrap();
        let content: String = (0..1405).map(|i| format!("// line {i}\n")).collect();
        tokio::fs::create_dir_all(dir.path().join("crates/router/src"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("crates/router/src/router.rs"), &content)
            .await
            .unwrap();

        let files = vec!["crates/router/src/router.rs".to_string()];
        let estimated = estimate_plan_tokens(dir.path(), &files, 3).await;

        // Budget gate uses config (default 80K) at 90% threshold = 72K.
        let budget = 80_000usize; // estimation test budget (lower than config default to validate gate)
        let threshold = budget * 90 / 100;
        assert!(
            estimated > threshold,
            "1405-line file must exceed 90% budget gate: estimated={estimated}, threshold={threshold}"
        );
    }

    #[tokio::test]
    async fn estimate_plan_tokens_subtask_catches_large_file() {
        // Sub-tasks use step_count=1, turns=3. A 1700-line file (13.6K tokens)
        // should be caught by the pre-flight check at 90% of 80K budget = 72K.
        let dir = tempfile::tempdir().unwrap();
        let content: String = (0..1700).map(|i| format!("// line {i}\n")).collect();
        tokio::fs::create_dir_all(dir.path().join("src"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("src/mod.rs"), &content)
            .await
            .unwrap();

        let files = vec!["src/mod.rs".to_string()];
        let estimated = estimate_plan_tokens(dir.path(), &files, 1).await;

        // Pre-flight uses 90% threshold for sub-tasks.
        let budget = 80_000usize;
        let threshold = budget * 90 / 100;
        assert!(
            estimated > threshold,
            "1700-line sub-task must exceed pre-flight: estimated={estimated}, threshold={threshold}"
        );
    }

    #[tokio::test]
    async fn estimate_plan_tokens_subtask_passes_medium_file() {
        // Sub-tasks with medium files (~500 lines) should pass pre-flight
        // now that the estimator accounts for read cache and fewer turns.
        let dir = tempfile::tempdir().unwrap();
        let content: String = (0..500).map(|i| format!("// line {i}\n")).collect();
        tokio::fs::create_dir_all(dir.path().join("src"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("src/small.rs"), &content)
            .await
            .unwrap();

        let files = vec!["src/small.rs".to_string()];
        let estimated = estimate_plan_tokens(dir.path(), &files, 1).await;

        // Pre-flight uses 90% threshold for sub-tasks.
        let budget = 80_000usize;
        let threshold = budget * 90 / 100;
        assert!(
            estimated < threshold,
            "500-line sub-task should pass pre-flight: estimated={estimated}, threshold={threshold}"
        );
    }

    // --- E2E pipeline test with Rust workspace structure ---

    #[tokio::test]
    async fn pipeline_e2e_rust_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // Set up a minimal Rust workspace structure.
        std::fs::write(
            dir.path().join("Cargo.toml"),
            r#"[workspace]
members = ["crates/mylib"]
resolver = "2"
"#,
        )
        .unwrap();

        std::fs::create_dir_all(dir.path().join("crates/mylib/src")).unwrap();
        std::fs::write(
            dir.path().join("crates/mylib/Cargo.toml"),
            r#"[package]
name = "mylib"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("crates/mylib/src/lib.rs"),
            r#"pub fn add(a: i32, b: i32) -> i32 { a + b }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_add() { assert_eq!(add(1, 2), 3); }
}
"#,
        )
        .unwrap();

        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add workspace"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-rust-ws",
                "Add a subtract function to crates/mylib/src/lib.rs",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // Verify the pipeline ran through all stages.
        assert!(
            result.status == PipelineStatus::Committed
                || result.status == PipelineStatus::PrCreated,
            "unexpected status: {:?}, error: {:?}",
            result.status,
            result.error,
        );

        // Verify indexer found Rust files.
        assert!(result.stage_outputs.contains_key("plan"));
        assert!(result.stage_outputs.contains_key("implement"));
        assert!(result.stage_outputs.contains_key("security"));
        assert!(result.stage_outputs.contains_key("release"));
        assert!(result.stage_outputs.contains_key("archive"));

        // Verify history was recorded.
        let history = JsonlHistory::new(dir.path());
        let query = glitchlab_memory::history::HistoryQuery {
            limit: 10,
            ..Default::default()
        };
        let entries = history.query(&query).await.unwrap_or_default();
        assert!(
            !entries.is_empty(),
            "history should have at least one entry"
        );

        // Verify events were emitted.
        assert!(!result.events.is_empty());
        assert!(
            result
                .events
                .iter()
                .any(|e| e.kind == EventKind::WorkspaceCreated),
            "expected WorkspaceCreated event"
        );
    }

    /// Live planner test — calls a real LLM provider.
    ///
    /// Only runs with: `cargo test -p glitchlab-eng-org --features live`
    /// Requires ANTHROPIC_API_KEY or another configured provider.
    #[cfg(feature = "live")]
    #[tokio::test]
    async fn live_planner_produces_valid_json() {
        use crate::agents::planner::PlannerAgent;
        use glitchlab_kernel::agent::Agent;
        use glitchlab_kernel::budget::BudgetTracker;

        let routing = HashMap::from([(
            "planner".to_string(),
            std::env::var("GLITCHLAB_PLANNER_MODEL")
                .unwrap_or_else(|_| "anthropic/claude-haiku-4-5-20251001".to_string()),
        )]);
        let budget = BudgetTracker::new(100_000, 1.0);
        let router = Arc::new(glitchlab_router::Router::new(routing, budget));

        let planner = PlannerAgent::new(router);
        let ctx = AgentContext {
            task_id: "live-test".into(),
            objective: "Add a greet() function to src/lib.rs that returns \"hello\"".into(),
            repo_path: "/tmp/live-test".into(),
            working_dir: "/tmp/live-test".into(),
            constraints: vec!["No new dependencies".into()],
            acceptance_criteria: vec![],
            risk_level: "low".into(),
            file_context: HashMap::new(),
            previous_output: serde_json::Value::Null,
            extra: HashMap::new(),
        };

        let output = planner.execute(&ctx).await.expect("planner should succeed");
        assert_eq!(output.metadata.agent, "planner");
        assert!(!output.parse_error, "should produce valid JSON");
        assert!(
            output.data["steps"].is_array(),
            "should have steps array: {:?}",
            output.data
        );
    }

    #[tokio::test]
    async fn pipeline_triage_already_done_skips_implementation() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — already_done
            final_response(
                r#"{"verdict": "already_done", "confidence": 0.95, "reasoning": "feature exists", "evidence": ["src/new.rs already has greet()"], "architectural_notes": "", "suggested_changes": []}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-already-done",
                "Add greet function",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::AlreadyDone);
        assert!(result.error.is_none());
        // Implementer should NOT have been called.
        assert!(!result.stage_outputs.contains_key("implement"));
        // Triage should be present.
        assert!(result.stage_outputs.contains_key("architect_triage"));
        // Should have triage event.
        assert!(
            result
                .events
                .iter()
                .any(|e| e.kind == EventKind::ArchitectTriage),
            "expected ArchitectTriage event"
        );
    }

    #[tokio::test]
    async fn pipeline_review_close_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed
            final_response(
                r#"{"verdict": "proceed", "confidence": 0.9, "reasoning": "not done", "evidence": [], "architectural_notes": "", "suggested_changes": []}"#,
            ),
            // 3. Implementer — write_file
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/new.rs", "content": "pub fn greet() {}\n"}),
            }]),
            // 4. Implementer — final
            final_response(
                r#"{"files_changed": ["src/new.rs"], "tests_added": [], "commit_message": "feat: greet", "summary": "done"}"#,
            ),
            // 5. Security
            final_response(r#"{"verdict": "pass", "issues": [], "summary": "ok"}"#),
            // 6. CISO
            final_response(
                r#"{"risk_verdict": "accept", "risk_score": 2, "blast_radius": "isolated", "trust_boundary_crossings": [], "data_flow_concerns": [], "compliance_flags": [], "operational_risk": {"rollback_complexity": "trivial", "monitoring_gaps": [], "failure_modes": []}, "aggregate_assessment": "low risk", "conditions": [], "escalation_reason": null}"#,
            ),
            // 7. Release
            final_response(
                r#"{"version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": []}"#,
            ),
            // 8. Archivist
            final_response(
                r#"{"adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#,
            ),
            // 9. Architect review — close
            final_response(
                r#"{"verdict": "close", "confidence": 0.8, "reasoning": "fundamentally wrong", "quality_score": 2, "issues": [], "architectural_fitness": "poor", "merge_strategy": "squash"}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-review-close",
                "Add greet function",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::ArchitectRejected);
        assert!(result.error.is_some());
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("architect review rejected")
        );
    }

    #[tokio::test]
    async fn pipeline_review_request_changes_stops() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed
            final_response(
                r#"{"verdict": "proceed", "confidence": 0.9, "reasoning": "not done", "evidence": [], "architectural_notes": "", "suggested_changes": []}"#,
            ),
            // 3. Implementer — write_file
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/new.rs", "content": "pub fn greet() {}\n"}),
            }]),
            // 4. Implementer — final
            final_response(
                r#"{"files_changed": ["src/new.rs"], "tests_added": [], "commit_message": "feat: greet", "summary": "done"}"#,
            ),
            // 5. Security
            final_response(r#"{"verdict": "pass", "issues": [], "summary": "ok"}"#),
            // 6. CISO
            final_response(
                r#"{"risk_verdict": "accept", "risk_score": 2, "blast_radius": "isolated", "trust_boundary_crossings": [], "data_flow_concerns": [], "compliance_flags": [], "operational_risk": {"rollback_complexity": "trivial", "monitoring_gaps": [], "failure_modes": []}, "aggregate_assessment": "low risk", "conditions": [], "escalation_reason": null}"#,
            ),
            // 7. Release
            final_response(
                r#"{"version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": []}"#,
            ),
            // 8. Archivist
            final_response(
                r#"{"adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#,
            ),
            // 9. Architect review — request_changes
            final_response(
                r#"{"verdict": "request_changes", "confidence": 0.7, "reasoning": "needs tests", "quality_score": 5, "issues": [{"severity": "medium", "description": "no tests", "suggestion": "add unit tests"}], "architectural_fitness": "acceptable", "merge_strategy": "squash"}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-review-changes",
                "Add greet function",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::Retryable);
        assert!(result.error.is_some());
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("architect requested changes")
        );
        assert!(result.branch.is_some());
        assert!(result.pr_url.is_none());
        assert!(result.outcome_context.is_some());
    }

    #[tokio::test]
    async fn pipeline_decomposition_returns_early() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner — returns a decomposition array
            final_response(
                r#"{"steps": [], "files_likely_affected": ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "large", "dependencies_affected": false, "public_api_changed": false, "decomposition": [{"id": "sub-1", "description": "sub-task 1"}, {"id": "sub-2", "description": "sub-task 2"}]}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-decompose",
                "Build a complex feature",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::Decomposed);
        assert!(result.error.is_none());
        assert!(result.stage_outputs.contains_key("plan"));
        // No implementation should have been attempted.
        assert!(!result.stage_outputs.contains_key("implement"));
        assert!(!result.stage_outputs.contains_key("architect_triage"));
    }

    #[tokio::test]
    async fn pipeline_budget_gate_forces_decomposition() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // Mirrors the actual canary failure: a 1400-line file that blew budget.
        // Quadratic model (step_count=3, turns=6, file_tokens=11200):
        //   6 * (5K+11200+2K) + 4K*15 = 109,200 + 60,000 = 169,200 > 72K (90% of 80K)
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let big_content: String = (0..1_400).map(|i| format!("// line {i}\n")).collect();
        std::fs::write(dir.path().join("src/big.rs"), &big_content).unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add big file"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let responses = vec![
            // 1. Planner — small complexity, but references a huge file
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add tracing"}, {"step_number": 2, "description": "add metrics"}, {"step_number": 3, "description": "test"}], "files_likely_affected": ["src/big.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": ["test tracing"], "estimated_complexity": "small", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Budget-aware replan — produces decomposition
            final_response(
                r#"{"steps": [], "files_likely_affected": ["src/big.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "medium", "dependencies_affected": false, "public_api_changed": false, "decomposition": [{"id": "p1", "objective": "add tracing to lines 1-200", "files_likely_affected": ["src/big.rs"]}, {"id": "p2", "objective": "add metrics to lines 300-400", "files_likely_affected": ["src/big.rs"]}]}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.limits.max_tokens_per_task = 80_000; // low budget to trigger gate

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-budget-gate",
                "Add tracing to src/big.rs",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(
            result.status,
            PipelineStatus::Decomposed,
            "budget gate should force decomposition, got {:?}, error: {:?}",
            result.status,
            result.error,
        );
        // Should have 2 PlanCreated events (original + budget-aware replan).
        let plan_created_count = result
            .events
            .iter()
            .filter(|e| e.kind == EventKind::PlanCreated)
            .count();
        assert_eq!(
            plan_created_count, 2,
            "expected 2 PlanCreated events (original + budget replan), got {plan_created_count}"
        );
    }

    #[tokio::test]
    async fn pipeline_budget_gate_passes_small_plan() {
        // A plan with small files should pass the budget gate and proceed normally.
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // Create a small file — well under budget threshold.
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/small.rs"),
            "pub fn hello() -> &'static str { \"hi\" }\n",
        )
        .unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add small file"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let responses = vec![
            // 1. Planner — small task, small file (should NOT trigger budget gate)
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature"}], "files_likely_affected": ["src/small.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": ["test"], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Implementer — write_file
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/small.rs", "content": "pub fn hello() -> &'static str { \"hi\" }\npub fn world() {}\n"}),
            }]),
            // 3. Implementer — final
            final_response(
                r#"{"files_changed": ["src/small.rs"], "tests_added": [], "commit_message": "feat: add world", "summary": "done", "tests_passing": true}"#,
            ),
            // 4. Security
            final_response(r#"{"verdict": "pass", "issues": [], "summary": "ok"}"#),
            // 5. CISO
            final_response(
                r#"{"risk_verdict": "accept", "risk_score": 2, "blast_radius": "isolated", "trust_boundary_crossings": [], "data_flow_concerns": [], "compliance_flags": [], "operational_risk": {"rollback_complexity": "trivial", "monitoring_gaps": [], "failure_modes": []}, "aggregate_assessment": "low risk", "conditions": [], "escalation_reason": null}"#,
            ),
            // 6. Release
            final_response(
                r#"{"version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": []}"#,
            ),
            // 7. Archivist
            final_response(
                r#"{"adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#,
            ),
            // 8. Architect review — approve
            final_response(
                r#"{"verdict": "approve", "confidence": 0.9, "reasoning": "looks good", "quality_score": 8, "issues": [], "architectural_fitness": "good", "merge_strategy": "squash"}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-budget-pass",
                "Add world function to src/small.rs",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // Should NOT be decomposed — budget estimate is well under threshold.
        assert_ne!(
            result.status,
            PipelineStatus::Decomposed,
            "small plan should not trigger budget gate decomposition"
        );
        // Should have exactly 1 PlanCreated event (no replan).
        let plan_created_count = result
            .events
            .iter()
            .filter(|e| e.kind == EventKind::PlanCreated)
            .count();
        assert_eq!(
            plan_created_count, 1,
            "expected 1 PlanCreated event (no budget replan), got {plan_created_count}"
        );
    }

    #[tokio::test]
    async fn pipeline_subtask_preflight_rejects_oversized() {
        // Sub-tasks (decomposition_depth > 0) skip the planner. The pre-flight
        // check should fail fast if the files are too large for the budget.
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // Create a large file that exceeds the budget pre-flight.
        // With the efficiency-adjusted estimator (turns=3, base=5K, growth=4K),
        // a file needs ~1700+ lines to exceed 90% of 80K = 72K.
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let big_content: String = (0..2_000).map(|i| format!("// line {i}\n")).collect();
        std::fs::write(dir.path().join("src/big.rs"), &big_content).unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add big file"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        // No LLM responses needed — should fail before any LLM call.
        let responses = vec![];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.limits.max_tokens_per_task = 80_000; // low budget to trigger pre-flight rejection

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        // Run as sub-task (depth=1) with file hints in the objective.
        let result = pipeline
            .run_with_depth(
                "test-subtask-preflight",
                "Add metrics struct\n\nFiles from parent task: src/big.rs",
                dir.path(),
                &base_branch,
                &[],
                1,
            )
            .await;

        assert_eq!(
            result.status,
            PipelineStatus::PlanFailed,
            "oversized sub-task should fail at pre-flight, got {:?}, error: {:?}",
            result.status,
            result.error,
        );
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("sub-task estimated at"),
            "error should mention sub-task budget: {:?}",
            result.error,
        );
    }

    #[tokio::test]
    async fn pipeline_subtask_preflight_passes_small() {
        // A sub-task with small files should pass the pre-flight check.
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/small.rs"), "pub fn x() {}\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add small file"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let responses = vec![
            // 1. Implementer — write_file
            tool_response(vec![ToolCall {
                id: "t1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/small.rs", "content": "pub fn x() {}\npub fn y() {}\n"}),
            }]),
            // 2. Implementer — final
            final_response(
                r#"{"files_changed": ["src/small.rs"], "tests_added": [], "commit_message": "feat: y", "summary": "done", "tests_passing": true}"#,
            ),
            // 3. Architect review — approve
            final_response(
                r#"{"verdict": "approve", "confidence": 0.9, "reasoning": "ok", "quality_score": 8, "issues": [], "architectural_fitness": "good", "merge_strategy": "squash"}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run_with_depth(
                "test-subtask-small",
                "Add y function\n\nFiles from parent task: src/small.rs",
                dir.path(),
                &base_branch,
                &[],
                1,
            )
            .await;

        assert_ne!(
            result.status,
            PipelineStatus::PlanFailed,
            "small sub-task should pass pre-flight, got {:?}, error: {:?}",
            result.status,
            result.error,
        );
    }

    #[tokio::test]
    async fn pipeline_triage_loads_affected_files() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // Create the file that the planner says is affected, so the triage
        // stage can load it into file_context.
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/existing.rs"),
            "pub fn greet() -> &'static str { \"hello\" }\n",
        )
        .unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add existing file"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let responses = vec![
            // 1. Planner — references files_likely_affected with a file on disk
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature"}], "files_likely_affected": ["src/existing.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — already_done (file already exists)
            final_response(
                r#"{"verdict": "already_done", "confidence": 0.95, "reasoning": "feature exists in src/existing.rs", "evidence": ["src/existing.rs"], "architectural_notes": "", "suggested_changes": []}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-triage-files",
                "Add greet function",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::AlreadyDone);
        assert!(result.stage_outputs.contains_key("architect_triage"));
    }

    #[tokio::test]
    async fn read_relevant_files_truncates_large_file() {
        let dir = tempfile::tempdir().unwrap();
        // Create a file larger than MAX_FILE_BYTES (8KB).
        let big_content = "x".repeat(MAX_FILE_BYTES + 100);
        tokio::fs::write(dir.path().join("big.rs"), &big_content)
            .await
            .unwrap();

        let indexed = vec!["big.rs".to_string()];
        let result = read_relevant_files(dir.path(), "Fix big.rs", &indexed).await;

        assert!(result.contains_key("big.rs"));
        let content = &result["big.rs"];
        assert!(content.ends_with("...\n[truncated]"));
        assert!(content.len() < big_content.len());
    }

    #[tokio::test]
    async fn pipeline_security_block_stops() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "high", "risk_notes": "risky", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed
            final_response(
                r#"{"verdict": "proceed", "confidence": 0.9, "reasoning": "not done", "evidence": [], "architectural_notes": "", "suggested_changes": []}"#,
            ),
            // 3. Implementer — write_file
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/new.rs", "content": "pub fn greet() {}\n"}),
            }]),
            // 4. Implementer — final
            final_response(
                r#"{"files_changed": ["src/new.rs"], "tests_added": [], "commit_message": "feat: greet", "summary": "done"}"#,
            ),
            // 5. Security — block
            final_response(
                r#"{"verdict": "block", "issues": [{"severity": "critical", "description": "SQL injection"}], "summary": "blocked"}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-sec-block",
                "Add greet function",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::SecurityBlocked);
        assert!(result.error.is_some());
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("security review blocked")
        );
    }

    #[tokio::test]
    async fn pipeline_implementer_stuck_bails() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed
            final_response(
                r#"{"verdict": "proceed", "confidence": 0.9, "reasoning": "not done", "evidence": [], "architectural_notes": "", "suggested_changes": []}"#,
            ),
            // 3. Implementer — write_file
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/new.rs", "content": "bad\n"}),
            }]),
            // 4. Implementer — final with stuck: true
            final_response(
                r#"{"stuck": true, "stuck_reason": "cannot resolve imports", "files_changed": [], "tests_added": [], "commit_message": "", "summary": ""}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-stuck",
                "Add greet function",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::ImplementationFailed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("cannot resolve imports")
        );
    }

    #[tokio::test]
    async fn pipeline_implementer_parse_error_bails() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed
            final_response(
                r#"{"verdict": "proceed", "confidence": 0.9, "reasoning": "not done", "evidence": [], "architectural_notes": "", "suggested_changes": []}"#,
            ),
            // 3. Implementer — write_file
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/new.rs", "content": "bad\n"}),
            }]),
            // 4. Implementer — garbled non-JSON (the sequential router will
            // return this; the implementer agent's parse logic will set
            // parse_error = true since it can't parse the JSON).
            final_response("this is not json at all"),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-parse-err",
                "Add greet function",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::ParseError);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("implementer failed: parse_error")
        );
    }

    #[tokio::test]
    async fn pipeline_boundary_violation_stops() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner — claims to touch a protected path
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "edit config"}], "files_likely_affected": ["secrets/keys.yaml"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed
            final_response(
                r#"{"verdict": "proceed", "confidence": 0.9, "reasoning": "not done", "evidence": [], "architectural_notes": "", "suggested_changes": []}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.boundaries.protected_paths = vec!["secrets/".into()];

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-boundary",
                "Edit secrets",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::BoundaryViolation);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn pipeline_injects_protected_paths_into_planner_context() {
        use glitchlab_kernel::agent::Message;
        use glitchlab_router::provider::{Provider, ProviderFuture};

        /// Mock provider that captures all messages sent to it and returns
        /// pre-scripted responses.
        struct CapturingProvider {
            responses: Mutex<VecDeque<glitchlab_router::RouterResponse>>,
            captured: Mutex<Vec<Vec<Message>>>,
        }

        impl CapturingProvider {
            fn new(responses: Vec<glitchlab_router::RouterResponse>) -> Self {
                Self {
                    responses: Mutex::new(VecDeque::from(responses)),
                    captured: Mutex::new(Vec::new()),
                }
            }
        }

        impl Provider for CapturingProvider {
            fn complete(
                &self,
                _model: &str,
                messages: &[Message],
                _temperature: f32,
                _max_tokens: u32,
                _response_format: Option<&serde_json::Value>,
            ) -> ProviderFuture<'_> {
                self.captured.lock().unwrap().push(messages.to_vec());
                let response = self
                    .responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| glitchlab_router::RouterResponse {
                        request_id: String::new(),
                        content: "no more responses".into(),
                        model: "cap/test".into(),
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                        cost: 0.0,
                        latency_ms: 0,
                        tool_calls: vec![],
                        stop_reason: None,
                    });
                Box::pin(async move { Ok(response) })
            }

            fn complete_with_tools(
                &self,
                model: &str,
                messages: &[Message],
                temperature: f32,
                max_tokens: u32,
                _tools: &[glitchlab_kernel::tool::ToolDefinition],
                response_format: Option<&serde_json::Value>,
            ) -> ProviderFuture<'_> {
                self.complete(model, messages, temperature, max_tokens, response_format)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner — a valid plan response (will touch protected path)
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "edit secrets"}], "files_likely_affected": ["secrets/keys.yaml"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed
            final_response(
                r#"{"verdict": "proceed", "confidence": 0.9, "reasoning": "ok", "evidence": [], "architectural_notes": "", "suggested_changes": []}"#,
            ),
        ];

        let capturing_provider = Arc::new(CapturingProvider::new(responses));
        let provider_clone = Arc::clone(&capturing_provider);

        let routing = std::collections::HashMap::from([
            ("planner".to_string(), "cap/test".to_string()),
            ("implementer".to_string(), "cap/test".to_string()),
            ("debugger".to_string(), "cap/test".to_string()),
            ("security".to_string(), "cap/test".to_string()),
            ("release".to_string(), "cap/test".to_string()),
            ("archivist".to_string(), "cap/test".to_string()),
            ("architect_triage".to_string(), "cap/test".to_string()),
            ("architect_review".to_string(), "cap/test".to_string()),
        ]);
        let budget = glitchlab_kernel::budget::BudgetTracker::new(1_000_000, 100.0);
        let mut router = glitchlab_router::Router::new(routing, budget);
        router.register_provider("cap".into(), capturing_provider);
        let router: RouterRef = Arc::new(router);

        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.boundaries.protected_paths = vec!["crates/kernel/".into(), "secrets/".into()];

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let _result = pipeline
            .run(
                "test-inject",
                "Modify kernel code",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // The first call to the provider is the planner. Its user message
        // (last message) should contain the protected paths section.
        let captured = provider_clone.captured.lock().unwrap();
        assert!(!captured.is_empty(), "expected at least one provider call");
        let planner_messages = &captured[0];
        let user_msg = planner_messages
            .iter()
            .find(|m| m.role == glitchlab_kernel::agent::MessageRole::User)
            .expect("planner call should have a user message");
        let text = match &user_msg.content {
            glitchlab_kernel::agent::MessageContent::Text(t) => t.clone(),
            _ => panic!("expected text content"),
        };
        assert!(
            text.contains("## Protected Paths"),
            "planner objective should contain protected paths header"
        );
        assert!(
            text.contains("crates/kernel/"),
            "planner objective should list crates/kernel/"
        );
        assert!(
            text.contains("secrets/"),
            "planner objective should list secrets/"
        );
    }

    #[tokio::test]
    async fn pipeline_tests_exhaust_max_fix_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // Makefile with test target that always fails.
        std::fs::write(
            dir.path().join("Makefile"),
            "test:\n\t@echo FAIL && exit 1\n",
        )
        .unwrap();
        std::process::Command::new("git")
            .args(["add", "Makefile"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add failing test"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let responses = vec![
            // 1. Planner
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "fix bug"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed
            final_response(
                r#"{"verdict": "proceed", "confidence": 0.9, "reasoning": "not done", "evidence": [], "architectural_notes": "", "suggested_changes": []}"#,
            ),
            // 3. Implementer — write_file
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/new.rs", "content": "pub fn greet() {}\n"}),
            }]),
            // 4. Implementer — final
            final_response(
                r#"{"files_changed": ["src/new.rs"], "tests_added": [], "commit_message": "fix: bug", "summary": "fixed"}"#,
            ),
            // 5. Debugger attempt 1 — should_retry: true (loop back to tests)
            final_response(
                r#"{"diagnosis": "test failure", "root_cause": "bug", "files_changed": [], "confidence": "low", "should_retry": true, "notes": null}"#,
            ),
            // 6. Debugger attempt 2 — should_retry: true (loop again, but max reached)
            final_response(
                r#"{"diagnosis": "still failing", "root_cause": "deeper bug", "files_changed": [], "confidence": "low", "should_retry": true, "notes": null}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.limits.max_fix_attempts = 2;

        let mock_ops = MockExternalOps::default();
        // 3 test failures: initial + 2 fix attempts = max exhausted.
        for _ in 0..3 {
            mock_ops
                .test_results
                .lock()
                .unwrap()
                .push_back(Err("FAIL\n".into()));
        }

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run(
                "test-exhaust-fixes",
                "Fix a bug",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::TestsFailed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("tests failing after 2 fix attempts")
        );
    }

    #[tokio::test]
    async fn pipeline_triage_truncates_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // Create a file larger than 8192 bytes so the triage truncation path hits.
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let big_content = "x".repeat(10000);
        std::fs::write(dir.path().join("src/big.rs"), &big_content).unwrap();
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add big file"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let responses = vec![
            // 1. Planner — references the big file
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "refactor big file"}], "files_likely_affected": ["src/big.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — already_done (short-circuits pipeline)
            final_response(
                r#"{"verdict": "already_done", "confidence": 0.95, "reasoning": "already refactored", "evidence": ["src/big.rs"], "architectural_notes": "", "suggested_changes": []}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-big-triage",
                "Refactor big file",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // The pipeline should complete (triage says already_done).
        // The truncation path in triage file loading is exercised.
        assert_eq!(result.status, PipelineStatus::AlreadyDone);
    }

    #[tokio::test]
    async fn pipeline_timeout_produces_timed_out_status() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let router = mock_router_ref();
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        // Set an impossibly short timeout.  With Duration::from_secs(0) the
        // timeout and the first async I/O in run_stages race each other, so
        // we accept *either* TimedOut (timeout won) or Error (workspace
        // creation completed first and the next step failed/timed-out).
        config.limits.max_pipeline_duration_secs = 0;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run("timeout-task", "do stuff", dir.path(), &base_branch, &[])
            .await;
        assert!(
            result.status == PipelineStatus::TimedOut || result.status == PipelineStatus::Error,
            "expected TimedOut or Error, got {:?}",
            result.status
        );
        assert!(
            result.error.is_some(),
            "should have an error message: {:?}",
            result.error
        );
    }

    // -------------------------------------------------------------------
    // ExternalOps post-commit path tests
    // -------------------------------------------------------------------

    /// Initialize a git repo with a local bare remote so `workspace.push()` succeeds.
    /// Returns `(base_branch, remote_tempdir)`. The remote tempdir must be kept alive.
    fn init_test_repo_with_remote(dir: &Path) -> (String, tempfile::TempDir) {
        let remote_dir = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "--bare"])
            .current_dir(remote_dir.path())
            .output()
            .unwrap();

        let base = init_test_repo(dir);

        std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                &remote_dir.path().display().to_string(),
            ])
            .current_dir(dir)
            .output()
            .unwrap();

        std::process::Command::new("git")
            .args(["push", "-u", "origin", &base])
            .current_dir(dir)
            .output()
            .unwrap();

        (base, remote_dir)
    }

    #[tokio::test]
    async fn pipeline_push_pr_merge_full_success() {
        let dir = tempfile::tempdir().unwrap();
        let (base_branch, _remote) = init_test_repo_with_remote(dir.path());

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.memory.beads_enabled = true;

        let mock_ops = MockExternalOps::default();
        mock_ops
            .create_pr_results
            .lock()
            .unwrap()
            .push_back(Ok("https://github.com/test/pr/1".into()));
        mock_ops.merge_results.lock().unwrap().push_back(Ok(()));
        mock_ops
            .close_bead_results
            .lock()
            .unwrap()
            .push_back(Ok(()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run("test-full", "Fix a bug", dir.path(), &base_branch, &[])
            .await;

        assert_eq!(result.status, PipelineStatus::PrMerged);
        assert_eq!(
            result.pr_url.as_deref(),
            Some("https://github.com/test/pr/1")
        );
        assert!(result.error.is_none());
        assert!(
            result.events.iter().any(|e| e.kind == EventKind::PrMerged),
            "expected PrMerged event"
        );
    }

    #[tokio::test]
    async fn pipeline_push_failure_returns_committed() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());
        // No remote → push fails.

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run("test-push-fail", "Fix a bug", dir.path(), &base_branch, &[])
            .await;

        assert_eq!(result.status, PipelineStatus::Committed);
        assert!(result.error.as_deref().unwrap().contains("push failed"));
        assert!(result.pr_url.is_none());
        assert!(result.branch.is_some());
    }

    #[tokio::test]
    async fn pipeline_pr_creation_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (base_branch, _remote) = init_test_repo_with_remote(dir.path());

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let mock_ops = MockExternalOps::default();
        mock_ops
            .create_pr_results
            .lock()
            .unwrap()
            .push_back(Err("gh pr create failed: not authenticated".into()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run("test-pr-fail", "Fix a bug", dir.path(), &base_branch, &[])
            .await;

        assert_eq!(result.status, PipelineStatus::Committed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("PR creation failed"),
        );
        assert!(result.pr_url.is_none());
    }

    #[tokio::test]
    async fn pipeline_pr_exists_reuses_url() {
        let dir = tempfile::tempdir().unwrap();
        let (base_branch, _remote) = init_test_repo_with_remote(dir.path());

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let mock_ops = MockExternalOps::default();
        // create_pr fails with "already exists"
        mock_ops.create_pr_results.lock().unwrap().push_back(Err(
            "gh pr create failed: a pull request already exists".into(),
        ));
        // view_existing_pr returns the existing URL
        mock_ops
            .view_pr_results
            .lock()
            .unwrap()
            .push_back(Ok("https://github.com/test/pr/42".into()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run("test-pr-exists", "Fix a bug", dir.path(), &base_branch, &[])
            .await;

        // Should succeed using the existing PR URL.
        assert!(
            result.status == PipelineStatus::PrCreated || result.status == PipelineStatus::PrMerged,
            "unexpected status: {:?}",
            result.status
        );
        assert_eq!(
            result.pr_url.as_deref(),
            Some("https://github.com/test/pr/42")
        );
    }

    #[tokio::test]
    async fn pipeline_merge_failure_pr_still_open() {
        let dir = tempfile::tempdir().unwrap();
        let (base_branch, _remote) = init_test_repo_with_remote(dir.path());

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let mock_ops = MockExternalOps::default();
        mock_ops
            .create_pr_results
            .lock()
            .unwrap()
            .push_back(Ok("https://github.com/test/pr/1".into()));
        mock_ops
            .merge_results
            .lock()
            .unwrap()
            .push_back(Err("gh pr merge failed: merge conflict".into()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run(
                "test-merge-fail",
                "Fix a bug",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // Merge failed → status stays PrCreated.
        assert_eq!(result.status, PipelineStatus::PrCreated);
        assert_eq!(
            result.pr_url.as_deref(),
            Some("https://github.com/test/pr/1")
        );
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn pipeline_bead_close_failure_nonfatal() {
        let dir = tempfile::tempdir().unwrap();
        let (base_branch, _remote) = init_test_repo_with_remote(dir.path());

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.memory.beads_enabled = true;

        let mock_ops = MockExternalOps::default();
        mock_ops
            .create_pr_results
            .lock()
            .unwrap()
            .push_back(Ok("https://github.com/test/pr/1".into()));
        mock_ops.merge_results.lock().unwrap().push_back(Ok(()));
        mock_ops
            .close_bead_results
            .lock()
            .unwrap()
            .push_back(Err("bd close failed: bead not found".into()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run("test-bead-fail", "Fix a bug", dir.path(), &base_branch, &[])
            .await;

        // Bead close failure is non-fatal — final status should still be PrMerged.
        assert_eq!(result.status, PipelineStatus::PrMerged);
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn pipeline_cargo_fmt_failure_nonfatal() {
        let dir = tempfile::tempdir().unwrap();
        let (base_branch, _remote) = init_test_repo_with_remote(dir.path());

        // Add Cargo.toml so the fmt path is triggered in the worktree.
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::process::Command::new("git")
            .args(["add", "Cargo.toml"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add Cargo.toml"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["push"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let mock_ops = MockExternalOps::default();
        mock_ops
            .fmt_results
            .lock()
            .unwrap()
            .push_back(Err("cargo fmt failed".into()));
        mock_ops
            .create_pr_results
            .lock()
            .unwrap()
            .push_back(Ok("https://github.com/test/pr/1".into()));
        mock_ops.merge_results.lock().unwrap().push_back(Ok(()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run("test-fmt-fail", "Fix a bug", dir.path(), &base_branch, &[])
            .await;

        // Fmt failure is best-effort — pipeline should still succeed.
        assert!(
            result.status == PipelineStatus::PrCreated || result.status == PipelineStatus::PrMerged,
            "unexpected status: {:?}, error: {:?}",
            result.status,
            result.error
        );
    }

    #[tokio::test]
    async fn pipeline_test_failure_triggers_debug_via_mock() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "fix bug"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed
            final_response(
                r#"{"verdict": "proceed", "confidence": 0.9, "reasoning": "not done", "evidence": [], "architectural_notes": "", "suggested_changes": []}"#,
            ),
            // 3. Implementer — write_file tool call
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/new.rs", "content": "pub fn greet() {}\n"}),
            }]),
            // 4. Implementer — final
            final_response(
                r#"{"files_changed": ["src/new.rs"], "tests_added": [], "commit_message": "fix: bug", "summary": "fixed"}"#,
            ),
            // 5. Debugger — should_retry: false
            final_response(
                r#"{"diagnosis": "test failure", "root_cause": "bug", "files_changed": [], "confidence": "low", "should_retry": false, "notes": null}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.test_command_override = Some("true".into()); // will be overridden by mock

        let mock_ops = MockExternalOps::default();
        // First test run fails, triggering debugger.
        mock_ops
            .test_results
            .lock()
            .unwrap()
            .push_back(Err("FAIL: assertion error".into()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run(
                "test-debug-mock",
                "Fix a bug",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::TestsFailed);
        assert!(
            result
                .events
                .iter()
                .any(|e| e.kind == EventKind::TestsFailed)
        );
        assert!(
            result
                .events
                .iter()
                .any(|e| e.kind == EventKind::DebugAttempt)
        );
        assert!(result.stage_outputs.contains_key("debug_1"));
    }

    #[tokio::test]
    async fn pipeline_test_success_skips_debug_via_mock() {
        let dir = tempfile::tempdir().unwrap();
        let (base_branch, _remote) = init_test_repo_with_remote(dir.path());

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.test_command_override = Some("true".into()); // will be overridden by mock

        let mock_ops = MockExternalOps::default();
        // Tests pass immediately.
        mock_ops.test_results.lock().unwrap().push_back(Ok(()));
        mock_ops
            .create_pr_results
            .lock()
            .unwrap()
            .push_back(Ok("https://github.com/test/pr/1".into()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run("test-pass-mock", "Fix a bug", dir.path(), &base_branch, &[])
            .await;

        assert!(
            result.status == PipelineStatus::PrCreated || result.status == PipelineStatus::PrMerged,
            "unexpected status: {:?}, error: {:?}",
            result.status,
            result.error
        );
        assert!(
            result
                .events
                .iter()
                .any(|e| e.kind == EventKind::TestsPassed),
            "expected TestsPassed event"
        );
        // No debug stage should exist.
        assert!(
            !result.stage_outputs.contains_key("debug_1"),
            "should not have debug stage"
        );
    }

    #[tokio::test]
    async fn pipeline_triage_needs_refinement_triggers_replan() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner — initial plan
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — needs_refinement
            final_response(
                r#"{"verdict": "needs_refinement", "confidence": 0.8, "reasoning": "plan missing error handling", "evidence": [], "architectural_notes": "", "suggested_changes": ["add error handling for edge cases"]}"#,
            ),
            // 3. Planner — revised plan (after replan)
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature with error handling"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 4. Implementer — write_file
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/new.rs", "content": "pub fn greet() -> Result<(), String> { Ok(()) }\n"}),
            }]),
            // 5. Implementer — final
            final_response(
                r#"{"files_changed": ["src/new.rs"], "tests_added": [], "commit_message": "feat: greet with error handling", "summary": "done"}"#,
            ),
            // 6. Security
            final_response(r#"{"verdict": "pass", "issues": [], "summary": "ok"}"#),
            // 7. CISO
            final_response(
                r#"{"risk_verdict": "accept", "risk_score": 2, "blast_radius": "isolated", "trust_boundary_crossings": [], "data_flow_concerns": [], "compliance_flags": [], "operational_risk": {"rollback_complexity": "trivial", "monitoring_gaps": [], "failure_modes": []}, "aggregate_assessment": "low risk", "conditions": [], "escalation_reason": null}"#,
            ),
            // 8. Release
            final_response(
                r#"{"version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": []}"#,
            ),
            // 9. Archivist
            final_response(
                r#"{"adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#,
            ),
            // 10. Architect review — merge
            final_response(
                r#"{"verdict": "merge", "confidence": 0.9, "reasoning": "good", "quality_score": 8, "issues": [], "architectural_fitness": "good", "merge_strategy": "squash"}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-needs-refinement",
                "Add greet function",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // Should have proceeded past triage (planner called twice).
        assert!(
            result.stage_outputs.contains_key("architect_triage"),
            "expected architect_triage stage output"
        );
        assert!(
            result.stage_outputs.contains_key("plan"),
            "expected plan stage output"
        );
        // Two PlanCreated events: original + replan.
        let plan_events: Vec<_> = result
            .events
            .iter()
            .filter(|e| e.kind == EventKind::PlanCreated)
            .collect();
        assert_eq!(
            plan_events.len(),
            2,
            "expected 2 PlanCreated events (original + replan), got {}",
            plan_events.len()
        );
        // Implementation should have been attempted (we got past triage).
        assert!(
            result.stage_outputs.contains_key("implement"),
            "expected implement stage output — replan should have proceeded to implementation"
        );
    }

    #[tokio::test]
    async fn pipeline_review_request_changes_returns_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed
            final_response(
                r#"{"verdict": "proceed", "confidence": 0.9, "reasoning": "not done", "evidence": [], "architectural_notes": "", "suggested_changes": []}"#,
            ),
            // 3. Implementer — write_file
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/new.rs", "content": "pub fn greet() {}\n"}),
            }]),
            // 4. Implementer — final
            final_response(
                r#"{"files_changed": ["src/new.rs"], "tests_added": [], "commit_message": "feat: greet", "summary": "done"}"#,
            ),
            // 5. Security
            final_response(r#"{"verdict": "pass", "issues": [], "summary": "ok"}"#),
            // 6. CISO
            final_response(
                r#"{"risk_verdict": "accept", "risk_score": 2, "blast_radius": "isolated", "trust_boundary_crossings": [], "data_flow_concerns": [], "compliance_flags": [], "operational_risk": {"rollback_complexity": "trivial", "monitoring_gaps": [], "failure_modes": []}, "aggregate_assessment": "low risk", "conditions": [], "escalation_reason": null}"#,
            ),
            // 7. Release
            final_response(
                r#"{"version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": []}"#,
            ),
            // 8. Archivist
            final_response(
                r#"{"adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#,
            ),
            // 9. Architect review — request_changes with issues
            final_response(
                r#"{"verdict": "request_changes", "confidence": 0.7, "reasoning": "needs tests and error handling", "quality_score": 4, "issues": [{"severity": "high", "description": "no tests", "suggestion": "add unit tests"}, {"severity": "medium", "description": "no error handling", "suggestion": "add Result return type"}], "architectural_fitness": "poor", "merge_strategy": "squash"}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-review-retryable",
                "Add greet function",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(
            result.status,
            PipelineStatus::Retryable,
            "expected Retryable, got {:?}",
            result.status
        );
        assert!(result.error.is_some());
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("architect requested changes"),
            "error should mention architect requested changes"
        );
        assert!(result.branch.is_some());
        assert!(result.pr_url.is_none());

        // outcome_context should be populated with architect feedback
        let oc = result
            .outcome_context
            .expect("outcome_context should be populated");
        assert!(
            oc.approach.contains("architect requested changes"),
            "approach should mention architect"
        );
        assert!(
            matches!(
                oc.obstacle,
                glitchlab_kernel::outcome::ObstacleKind::ArchitecturalGap { .. }
            ),
            "obstacle should be ArchitecturalGap"
        );
        assert!(
            oc.recommendation.is_some(),
            "recommendation should contain architect reasoning"
        );
    }

    // -----------------------------------------------------------------------
    // Post-PR architect review tests
    // -----------------------------------------------------------------------

    /// Build mock responses for a full pipeline run that includes a post-PR
    /// architect review step. The 9th response is the post-PR review verdict.
    fn pipeline_mock_responses_with_pr_review(
        verdict: &str,
    ) -> Vec<glitchlab_router::RouterResponse> {
        let mut responses = pipeline_mock_responses();
        // 10. Post-PR architect review
        responses.push(final_response(&format!(
            r#"{{"verdict": "{verdict}", "confidence": 0.9, "reasoning": "post-pr review", "quality_score": 8, "issues": [{{"severity": "minor", "description": "nit"}}], "architectural_fitness": "good", "merge_strategy": "squash"}}"#
        )));
        responses
    }

    #[tokio::test]
    async fn pipeline_pr_review_approve_merges() {
        let dir = tempfile::tempdir().unwrap();
        let (base_branch, _remote) = init_test_repo_with_remote(dir.path());

        let router = sequential_router_ref(pipeline_mock_responses_with_pr_review("merge"));
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.intervention.review_pr_diff = true;

        let mock_ops = MockExternalOps::default();
        mock_ops
            .create_pr_results
            .lock()
            .unwrap()
            .push_back(Ok("https://github.com/test/pr/1".into()));
        mock_ops
            .pr_diff_results
            .lock()
            .unwrap()
            .push_back(Ok("diff --git a/f.rs b/f.rs\n+line\n".into()));
        mock_ops.merge_results.lock().unwrap().push_back(Ok(()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run(
                "test-pr-review-approve",
                "Fix a bug",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::PrMerged);
        assert_eq!(
            result.pr_url.as_deref(),
            Some("https://github.com/test/pr/1")
        );
        assert!(
            result
                .events
                .iter()
                .any(|e| e.kind == EventKind::ArchitectPrReview),
            "expected ArchitectPrReview event"
        );
        assert!(
            result.events.iter().any(|e| e.kind == EventKind::PrMerged),
            "expected PrMerged event"
        );
    }

    #[tokio::test]
    async fn pipeline_pr_review_request_changes_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let (base_branch, _remote) = init_test_repo_with_remote(dir.path());

        let router =
            sequential_router_ref(pipeline_mock_responses_with_pr_review("request_changes"));
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.intervention.review_pr_diff = true;

        let mock_ops = MockExternalOps::default();
        mock_ops
            .create_pr_results
            .lock()
            .unwrap()
            .push_back(Ok("https://github.com/test/pr/2".into()));
        mock_ops
            .pr_diff_results
            .lock()
            .unwrap()
            .push_back(Ok("diff --git a/f.rs b/f.rs\n+line\n".into()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run(
                "test-pr-review-changes",
                "Fix a bug",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::Retryable);
        assert_eq!(
            result.pr_url.as_deref(),
            Some("https://github.com/test/pr/2"),
            "pr_url should be preserved on request_changes"
        );
        assert!(result.outcome_context.is_some());
        let oc = result.outcome_context.unwrap();
        assert!(
            oc.approach.contains("post-PR"),
            "approach should mention post-PR"
        );
    }

    #[tokio::test]
    async fn pipeline_pr_review_close_rejects_with_pr_url() {
        let dir = tempfile::tempdir().unwrap();
        let (base_branch, _remote) = init_test_repo_with_remote(dir.path());

        let router = sequential_router_ref(pipeline_mock_responses_with_pr_review("close"));
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.intervention.review_pr_diff = true;

        let mock_ops = MockExternalOps::default();
        mock_ops
            .create_pr_results
            .lock()
            .unwrap()
            .push_back(Ok("https://github.com/test/pr/3".into()));
        mock_ops
            .pr_diff_results
            .lock()
            .unwrap()
            .push_back(Ok("diff --git a/f.rs b/f.rs\n+line\n".into()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run(
                "test-pr-review-close",
                "Fix a bug",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::ArchitectRejected);
        assert_eq!(
            result.pr_url.as_deref(),
            Some("https://github.com/test/pr/3"),
            "pr_url should be preserved on close"
        );
        assert!(result.error.as_ref().is_some_and(|e| e.contains("post-PR")));
    }

    #[tokio::test]
    async fn pipeline_pr_review_diff_fetch_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let (base_branch, _remote) = init_test_repo_with_remote(dir.path());

        let router = sequential_router_ref(pipeline_mock_responses_with_pr_review("merge"));
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.intervention.review_pr_diff = true;

        let mock_ops = MockExternalOps::default();
        mock_ops
            .create_pr_results
            .lock()
            .unwrap()
            .push_back(Ok("https://github.com/test/pr/4".into()));
        // pr_diff fails → should fallback to local diff
        mock_ops
            .pr_diff_results
            .lock()
            .unwrap()
            .push_back(Err("gh not available".into()));
        mock_ops.merge_results.lock().unwrap().push_back(Ok(()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run(
                "test-pr-review-fallback",
                "Fix a bug",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // Should still succeed (merge verdict) using fallback diff.
        assert_eq!(result.status, PipelineStatus::PrMerged);
        assert!(
            result
                .events
                .iter()
                .any(|e| e.kind == EventKind::ArchitectPrReview),
            "expected ArchitectPrReview event even when using fallback diff"
        );
    }

    #[tokio::test]
    async fn pipeline_pr_review_disabled_skips_review() {
        let dir = tempfile::tempdir().unwrap();
        let (base_branch, _remote) = init_test_repo_with_remote(dir.path());

        // Standard responses — no extra post-PR review response needed.
        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.intervention.review_pr_diff = false; // disabled

        let mock_ops = MockExternalOps::default();
        mock_ops
            .create_pr_results
            .lock()
            .unwrap()
            .push_back(Ok("https://github.com/test/pr/5".into()));
        mock_ops.merge_results.lock().unwrap().push_back(Ok(()));

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, Arc::new(mock_ops));

        let result = pipeline
            .run(
                "test-pr-review-disabled",
                "Fix a bug",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::PrMerged);
        // No ArchitectPrReview event should exist.
        assert!(
            !result
                .events
                .iter()
                .any(|e| e.kind == EventKind::ArchitectPrReview),
            "ArchitectPrReview event should not be emitted when review_pr_diff is disabled"
        );
    }

    #[tokio::test]
    async fn real_ops_pr_diff_without_gh() {
        let dir = tempfile::tempdir().unwrap();
        let ops = RealExternalOps;
        // `gh` likely not configured for this repo → should return an error.
        let result = ops
            .pr_diff(dir.path(), "https://github.com/test/pr/999")
            .await;
        assert!(result.is_err(), "expected error when gh is not available");
    }

    // --- extract_file_paths_from_text tests ---

    #[test]
    fn extract_paths_from_objective_with_explicit_file() {
        let text = "Add Display to OutcomeKind enum in crates/kernel/src/outcome.rs";
        let paths = extract_file_paths_from_text(text);
        assert!(
            paths.contains(&"crates/kernel/src/outcome.rs".to_string()),
            "should extract crates/kernel/src/outcome.rs, got: {paths:?}"
        );
    }

    #[test]
    fn extract_paths_from_backtick_wrapped() {
        let text = "Modify `crates/eng-org/src/pipeline.rs` to add feature";
        let paths = extract_file_paths_from_text(text);
        assert!(
            paths.contains(&"crates/eng-org/src/pipeline.rs".to_string()),
            "should extract backtick-wrapped path, got: {paths:?}"
        );
    }

    #[test]
    fn extract_paths_ignores_non_paths() {
        let text = "Fix the display bug and update README";
        let paths = extract_file_paths_from_text(text);
        assert!(
            paths.is_empty(),
            "should not extract non-paths, got: {paths:?}"
        );
    }

    #[test]
    fn extract_paths_directory_style() {
        let text = "Changes under crates/kernel/ are forbidden";
        let paths = extract_file_paths_from_text(text);
        assert!(
            paths.contains(&"crates/kernel".to_string()),
            "should extract directory-style path, got: {paths:?}"
        );
    }

    #[test]
    fn extract_paths_multiple() {
        let text = "Edit src/lib.rs and crates/router/src/mod.rs";
        let paths = extract_file_paths_from_text(text);
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"src/lib.rs".to_string()));
        assert!(paths.contains(&"crates/router/src/mod.rs".to_string()));
    }

    // --- extract_all_plan_files tests ---

    #[test]
    fn extract_plan_files_from_files_likely_affected() {
        let plan = serde_json::json!({
            "files_likely_affected": ["src/lib.rs", "src/main.rs"],
            "steps": []
        });
        let files = extract_all_plan_files(&plan);
        assert_eq!(files, vec!["src/lib.rs", "src/main.rs"]);
    }

    #[test]
    fn extract_plan_files_from_steps() {
        let plan = serde_json::json!({
            "files_likely_affected": ["src/lib.rs"],
            "steps": [
                {"files": ["src/lib.rs", "crates/kernel/src/outcome.rs"]},
                {"files": ["src/other.rs"]}
            ]
        });
        let files = extract_all_plan_files(&plan);
        assert!(
            files.contains(&"crates/kernel/src/outcome.rs".to_string()),
            "should include files from steps[].files, got: {files:?}"
        );
        assert!(files.contains(&"src/lib.rs".to_string()));
        assert!(files.contains(&"src/other.rs".to_string()));
    }

    #[test]
    fn extract_plan_files_deduplicates() {
        let plan = serde_json::json!({
            "files_likely_affected": ["src/lib.rs"],
            "steps": [{"files": ["src/lib.rs"]}]
        });
        let files = extract_all_plan_files(&plan);
        assert_eq!(files, vec!["src/lib.rs"]);
    }

    #[test]
    fn extract_plan_files_from_decomposition() {
        let plan = serde_json::json!({
            "files_likely_affected": [],
            "steps": [],
            "decomposition": [
                {"id": "task-1", "objective": "Modify crates/kernel/src/outcome.rs"},
                {"id": "task-2", "objective": "Update src/lib.rs"}
            ]
        });
        let files = extract_all_plan_files(&plan);
        assert!(
            files.contains(&"crates/kernel/src/outcome.rs".to_string()),
            "should extract paths from decomposition objectives, got: {files:?}"
        );
        assert!(files.contains(&"src/lib.rs".to_string()));
    }

    // --- Pre-planner boundary scan integration test ---

    #[tokio::test]
    async fn pre_planner_scan_rejects_protected_path_in_objective() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // No LLM responses needed — should reject before calling any agent.
        let router = mock_router_ref();

        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.boundaries.protected_paths = vec!["crates/kernel".into(), ".github".into()];

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-pre-scan",
                "Add Display to OutcomeKind in crates/kernel/src/outcome.rs",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(
            result.status,
            PipelineStatus::BoundaryViolation,
            "should reject at pre-planner scan, got: {:?} error: {:?}",
            result.status,
            result.error
        );
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("objective references protected path"),
            "error should mention objective, got: {:?}",
            result.error
        );
        // Budget should be near-zero — no LLM calls made.
        assert_eq!(
            result.budget.total_tokens, 0,
            "no LLM tokens should be consumed"
        );
    }

    #[tokio::test]
    async fn pre_planner_scan_allows_non_protected_objective() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // Pipeline will proceed to planner, so we need mock responses.
        let responses = pipeline_mock_responses();
        let router = sequential_router_ref(responses);

        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.boundaries.protected_paths = vec!["crates/kernel".into()];

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-non-protected",
                "Add a function to crates/eng-org/src/lib.rs",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // Should NOT be BoundaryViolation — the objective doesn't target a protected path.
        assert_ne!(
            result.status,
            PipelineStatus::BoundaryViolation,
            "non-protected objective should not trigger boundary violation"
        );
    }

    #[tokio::test]
    async fn stage4_catches_protected_path_in_step_files() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // Planner returns a plan where files_likely_affected is clean,
        // but steps[].files contains a protected path.
        let responses = vec![
            // 1. Planner — files_likely_affected is clean, but step files are not
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "edit kernel", "files": ["crates/kernel/src/outcome.rs"], "action": "modify"}], "files_likely_affected": ["src/lib.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed
            final_response(
                r#"{"verdict": "proceed", "confidence": 0.9, "reasoning": "ok", "evidence": [], "architectural_notes": "", "suggested_changes": []}"#,
            ),
        ];
        let router = sequential_router_ref(responses);

        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.boundaries.protected_paths = vec!["crates/kernel".into()];

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-step-files",
                // Objective doesn't mention the protected path, so pre-planner scan passes.
                "Add Display impl for OutcomeKind",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(
            result.status,
            PipelineStatus::BoundaryViolation,
            "Stage 4 should catch protected path in steps[].files, got: {:?} error: {:?}",
            result.status,
            result.error
        );
    }

    // --- short_circuit eligibility tests ---

    #[test]
    fn short_circuit_trivial_one_file() {
        let plan = serde_json::json!({
            "estimated_complexity": "trivial",
            "requires_core_change": false,
            "files_likely_affected": ["src/lib.rs"],
        });
        assert!(is_short_circuit_eligible(&plan, 2));
    }

    #[test]
    fn short_circuit_small_two_files() {
        let plan = serde_json::json!({
            "estimated_complexity": "small",
            "requires_core_change": false,
            "files_likely_affected": ["src/a.rs", "src/b.rs"],
        });
        assert!(is_short_circuit_eligible(&plan, 2));
    }

    #[test]
    fn short_circuit_medium_complexity_with_few_files_allowed() {
        // Sub-tasks from decomposition are structurally small but labelled
        // "medium" — they should still qualify for short-circuit.
        let plan = serde_json::json!({
            "estimated_complexity": "medium",
            "requires_core_change": false,
            "files_likely_affected": ["src/lib.rs"],
            "steps": [{"step_number": 1, "description": "edit"}],
        });
        assert!(is_short_circuit_eligible(&plan, 2));
    }

    #[test]
    fn short_circuit_large_complexity_rejected() {
        let plan = serde_json::json!({
            "estimated_complexity": "large",
            "requires_core_change": false,
            "files_likely_affected": ["src/lib.rs"],
        });
        assert!(!is_short_circuit_eligible(&plan, 2));
    }

    #[test]
    fn short_circuit_too_many_steps_rejected() {
        let plan = serde_json::json!({
            "estimated_complexity": "small",
            "requires_core_change": false,
            "files_likely_affected": ["src/lib.rs"],
            "steps": [
                {"step_number": 1}, {"step_number": 2},
                {"step_number": 3}, {"step_number": 4},
            ],
        });
        assert!(!is_short_circuit_eligible(&plan, 2));
    }

    #[test]
    fn short_circuit_core_change_rejected() {
        let plan = serde_json::json!({
            "estimated_complexity": "trivial",
            "requires_core_change": true,
            "files_likely_affected": ["src/lib.rs"],
        });
        assert!(!is_short_circuit_eligible(&plan, 2));
    }

    #[test]
    fn short_circuit_too_many_files_rejected() {
        let plan = serde_json::json!({
            "estimated_complexity": "trivial",
            "requires_core_change": false,
            "files_likely_affected": ["a.rs", "b.rs", "c.rs"],
        });
        assert!(!is_short_circuit_eligible(&plan, 2));
    }

    #[test]
    fn short_circuit_decomposed_rejected() {
        let plan = serde_json::json!({
            "estimated_complexity": "trivial",
            "requires_core_change": false,
            "files_likely_affected": ["src/lib.rs"],
            "decomposition": [{"objective": "sub-task 1"}],
        });
        assert!(!is_short_circuit_eligible(&plan, 2));
    }

    #[test]
    fn short_circuit_missing_fields_rejected() {
        let plan = serde_json::json!({});
        assert!(!is_short_circuit_eligible(&plan, 2));
    }

    #[test]
    fn short_circuit_empty_files_rejected() {
        let plan = serde_json::json!({
            "estimated_complexity": "trivial",
            "requires_core_change": false,
            "files_likely_affected": [],
        });
        assert!(!is_short_circuit_eligible(&plan, 2));
    }

    #[test]
    fn short_circuit_empty_decomposition_allowed() {
        let plan = serde_json::json!({
            "estimated_complexity": "small",
            "requires_core_change": false,
            "files_likely_affected": ["src/lib.rs"],
            "decomposition": [],
        });
        assert!(is_short_circuit_eligible(&plan, 2));
    }

    // --- build_rust_module_map tests ---

    #[test]
    fn module_map_extracts_pub_mod() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("crates/eng/src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("lib.rs"),
            "pub mod agents;\nmod config;\npub(crate) mod pipeline;\nuse std::io;\nfn helper() {}\n",
        )
        .unwrap();

        let files = vec!["crates/eng/src/agents/mod.rs".to_string()];
        let result = build_rust_module_map(dir.path(), &files);

        assert!(result.contains("## Rust Module Map"));
        assert!(result.contains("`pub mod agents;`"));
        assert!(result.contains("`mod config;`"));
        assert!(result.contains("`pub(crate) mod pipeline;`"));
        // Should not contain non-module lines.
        assert!(!result.contains("use std::io"));
        assert!(!result.contains("fn helper"));
    }

    #[test]
    fn module_map_handles_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let files = vec!["nonexistent/src/lib.rs".to_string()];
        let result = build_rust_module_map(dir.path(), &files);
        assert!(result.is_empty());
    }

    #[test]
    fn module_map_empty_for_non_rust() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<String> = vec![];
        let result = build_rust_module_map(dir.path(), &files);
        assert!(result.is_empty());
    }

    #[test]
    fn module_map_includes_mod_rs() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path().join("src/agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("mod.rs"),
            "pub mod planner;\npub mod implementer;\n",
        )
        .unwrap();

        let files = vec!["src/agents/planner.rs".to_string()];
        let result = build_rust_module_map(dir.path(), &files);

        assert!(result.contains("`pub mod planner;`"));
        assert!(result.contains("`pub mod implementer;`"));
    }

    #[test]
    fn module_map_skips_empty_module_files() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        // lib.rs exists but has no module declarations.
        std::fs::write(src.join("lib.rs"), "fn main() {}\n").unwrap();

        let files = vec!["src/foo.rs".to_string()];
        let result = build_rust_module_map(dir.path(), &files);
        assert!(result.is_empty());
    }

    // --- short-circuit pipeline integration tests ---

    /// Mock responses for a short-circuit pipeline run.
    ///
    /// Short circuit skips triage, security, release, and archivist, so the
    /// sequence is: planner → implementer (tool_use) → implementer (final) →
    /// architect review.
    fn short_circuit_mock_responses() -> Vec<glitchlab_router::RouterResponse> {
        vec![
            // 1. Planner — trivial, 1 file, no core change
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add comment", "files": ["src/lib.rs"], "action": "modify"}], "files_likely_affected": ["src/lib.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "trivial", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Implementer — edit_file tool call (no triage!)
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/lib.rs", "content": "// Added comment\npub fn greet() -> &'static str { \"hello\" }\n"}),
            }]),
            // 3. Implementer — final metadata
            final_response(
                r#"{"files_changed": ["src/lib.rs"], "tests_added": [], "commit_message": "docs: add comment", "summary": "added comment"}"#,
            ),
            // 4. Architect review — merge (no security/release/archivist!)
            final_response(
                r#"{"verdict": "merge", "confidence": 0.95, "reasoning": "trivial change", "quality_score": 9, "issues": [], "architectural_fitness": "good", "merge_strategy": "squash"}"#,
            ),
        ]
    }

    #[tokio::test]
    async fn pipeline_short_circuit_skips_stages() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let router = sequential_router_ref(short_circuit_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.pipeline.short_circuit_enabled = true;
        config.pipeline.short_circuit_max_files = 2;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-short-circuit",
                "Add a comment to lib.rs",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert!(
            result.status == PipelineStatus::Committed
                || result.status == PipelineStatus::PrCreated
                || result.status == PipelineStatus::PrMerged,
            "unexpected status: {:?}, error: {:?}",
            result.status,
            result.error
        );

        // Triage should NOT be in stage outputs (skipped).
        assert!(
            !result.stage_outputs.contains_key("architect_triage"),
            "triage should be skipped on short circuit"
        );

        // Archive should NOT be in stage outputs (skipped).
        assert!(
            !result.stage_outputs.contains_key("archive"),
            "archivist should be skipped on short circuit"
        );

        // Security and release should exist but with synthetic "skipped" outputs.
        let security = result.stage_outputs.get("security").unwrap();
        assert_eq!(
            security.data["verdict"].as_str().unwrap(),
            "skipped",
            "security verdict should be 'skipped'"
        );

        let release = result.stage_outputs.get("release").unwrap();
        assert_eq!(
            release.data["version_bump"].as_str().unwrap(),
            "patch",
            "release should default to 'patch'"
        );
        assert_eq!(
            release.data["reasoning"].as_str().unwrap(),
            "skipped (short circuit)",
        );

        // Plan and implement should still be present.
        assert!(result.stage_outputs.contains_key("plan"));
        assert!(result.stage_outputs.contains_key("implement"));
    }

    #[tokio::test]
    async fn pipeline_short_circuit_disabled_runs_all_stages() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // Use the full response set — short circuit is disabled by default.
        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        // Explicitly disabled (the default).
        config.pipeline.short_circuit_enabled = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-no-short-circuit",
                "Fix a bug",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert!(
            result.status == PipelineStatus::Committed
                || result.status == PipelineStatus::PrCreated
                || result.status == PipelineStatus::PrMerged,
            "unexpected status: {:?}, error: {:?}",
            result.status,
            result.error
        );

        // All stages should be present when short circuit is off.
        assert!(result.stage_outputs.contains_key("architect_triage"));
        assert!(result.stage_outputs.contains_key("security"));
        assert!(result.stage_outputs.contains_key("ciso"));
        assert!(result.stage_outputs.contains_key("release"));
        assert!(result.stage_outputs.contains_key("archive"));

        // Security should have a real verdict, not "skipped".
        let security = result.stage_outputs.get("security").unwrap();
        assert_ne!(
            security.data["verdict"].as_str().unwrap_or(""),
            "skipped",
            "security should run when short circuit is disabled"
        );
    }

    #[tokio::test]
    async fn pipeline_short_circuit_not_eligible_runs_all_stages() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        // Full responses needed — 3 files exceeds max_files so short circuit won't trigger.
        let mut responses = pipeline_mock_responses();
        // Replace planner response with medium complexity.
        responses[0] = final_response(
            r#"{"steps": [{"step_number": 1, "description": "refactor module", "files": ["src/a.rs", "src/b.rs", "src/c.rs"], "action": "modify"}], "files_likely_affected": ["src/a.rs", "src/b.rs", "src/c.rs"], "requires_core_change": false, "risk_level": "medium", "risk_notes": "refactor", "test_strategy": [], "estimated_complexity": "medium", "dependencies_affected": false, "public_api_changed": false}"#,
        );
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.pipeline.short_circuit_enabled = true; // Enabled, but won't qualify.

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-not-eligible",
                "Refactor the module",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert!(
            result.status == PipelineStatus::Committed
                || result.status == PipelineStatus::PrCreated
                || result.status == PipelineStatus::PrMerged,
            "unexpected status: {:?}, error: {:?}",
            result.status,
            result.error
        );

        // 3 files exceeds max_files (2) → not eligible → all stages run.
        assert!(
            result.stage_outputs.contains_key("architect_triage"),
            "triage should run for non-eligible tasks"
        );
        assert!(
            result.stage_outputs.contains_key("archive"),
            "archivist should run for non-eligible tasks"
        );
    }

    // --- task sizing tests ---

    /// Mock responses for a pipeline run with triage returning task_size "S".
    /// Same as `pipeline_mock_responses()` but triage includes `task_size`.
    fn pipeline_mock_responses_with_task_size(
        task_size: &str,
    ) -> Vec<glitchlab_router::RouterResponse> {
        vec![
            // 1. Planner
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature", "files": ["src/new.rs"], "action": "create"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "trivial", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — proceed with task_size
            final_response(&format!(
                r#"{{"verdict": "proceed", "task_size": "{task_size}", "sizing_rationale": "test", "confidence": 0.9, "reasoning": "ok", "evidence": [], "architectural_notes": "", "suggested_changes": []}}"#,
            )),
            // 3. Implementer — write_file tool call
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/new.rs", "content": "pub fn greet() {}\n"}),
            }]),
            // 4. Implementer — final metadata
            final_response(
                r#"{"files_changed": ["src/new.rs"], "tests_added": [], "commit_message": "feat: add greet", "summary": "done"}"#,
            ),
            // 5. Security
            final_response(r#"{"verdict": "pass", "issues": [], "summary": "no issues"}"#),
            // 6. CISO
            final_response(
                r#"{"risk_verdict": "accept", "risk_score": 2, "blast_radius": "isolated", "trust_boundary_crossings": [], "data_flow_concerns": [], "compliance_flags": [], "operational_risk": {"rollback_complexity": "trivial", "monitoring_gaps": [], "failure_modes": []}, "aggregate_assessment": "low risk", "conditions": [], "escalation_reason": null}"#,
            ),
            // 7. Release
            final_response(
                r#"{"version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": []}"#,
            ),
            // 8. Archivist
            final_response(
                r#"{"adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#,
            ),
            // 9. Architect review — merge
            final_response(
                r#"{"verdict": "merge", "confidence": 0.9, "reasoning": "good", "quality_score": 8, "issues": [], "architectural_fitness": "good", "merge_strategy": "squash"}"#,
            ),
        ]
    }

    #[tokio::test]
    async fn pipeline_triage_sizing_applies_budget() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let router = sequential_router_ref(pipeline_mock_responses_with_task_size("S"));
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router.clone(), config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-sizing-s",
                "Add a small function",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert!(
            result.status == PipelineStatus::Committed
                || result.status == PipelineStatus::PrCreated
                || result.status == PipelineStatus::PrMerged,
            "unexpected status: {:?}, error: {:?}",
            result.status,
            result.error
        );

        // Budget effective limit = S ceiling (40k) + planner/triage overhead.
        // tokens_remaining = effective_limit - total_used. Since overhead is
        // added to the ceiling, implementer gets the full S budget.
        let summary = router.budget_summary().await;
        assert!(
            summary.tokens_remaining <= 41_000,
            "budget should be ~S (40k + overhead), got remaining: {}",
            summary.tokens_remaining
        );
    }

    #[tokio::test]
    async fn pipeline_triage_sizing_l() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let router = sequential_router_ref(pipeline_mock_responses_with_task_size("L"));
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router.clone(), config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-sizing-l",
                "Implement new module",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert!(
            result.status == PipelineStatus::Committed
                || result.status == PipelineStatus::PrCreated
                || result.status == PipelineStatus::PrMerged,
            "unexpected status: {:?}, error: {:?}",
            result.status,
            result.error
        );

        // Budget effective limit = L ceiling (200k) + planner/triage overhead.
        let summary = router.budget_summary().await;
        assert!(
            summary.tokens_remaining <= 201_000,
            "budget should be ~L (200k + overhead), got remaining: {}",
            summary.tokens_remaining
        );
    }

    #[tokio::test]
    async fn pipeline_triage_xl_forces_decomposition() {
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner — multi-file task
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "refactor", "files": ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"], "action": "modify"}], "files_likely_affected": ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"], "requires_core_change": false, "risk_level": "high", "risk_notes": "big change", "test_strategy": [], "estimated_complexity": "large", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — XL (forces decomposition)
            final_response(
                r#"{"verdict": "proceed", "task_size": "XL", "sizing_rationale": "4+ files", "confidence": 0.9, "reasoning": "too large", "evidence": [], "architectural_notes": "", "suggested_changes": ["decompose"]}"#,
            ),
            // 3. Re-plan (triggered by XL) — produces decomposition
            final_response(
                r#"{"steps": [], "files_likely_affected": ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"], "requires_core_change": false, "risk_level": "medium", "risk_notes": "decomposed", "test_strategy": [], "estimated_complexity": "large", "dependencies_affected": false, "public_api_changed": false, "decomposition": [{"id": "t1-part1", "objective": "refactor a.rs", "files_likely_affected": ["src/a.rs"], "depends_on": []}, {"id": "t1-part2", "objective": "refactor b.rs", "files_likely_affected": ["src/b.rs"], "depends_on": ["t1-part1"]}]}"#,
            ),
        ];

        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-xl-decompose",
                "Large refactor across 4 files",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(
            result.status,
            PipelineStatus::Decomposed,
            "XL task should be decomposed, got {:?}, error: {:?}",
            result.status,
            result.error
        );

        // Plan should contain the decomposition.
        let plan = result.stage_outputs.get("plan").unwrap();
        assert!(
            plan.data.get("decomposition").is_some(),
            "decomposed plan should have decomposition field"
        );
    }

    #[tokio::test]
    async fn pipeline_xl_guard_blocks_implementer() {
        // When an XL task fails to decompose (re-plan doesn't produce a
        // decomposition array), the pipeline should return PlanFailed
        // instead of falling through to the implementer.
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner — multi-file task
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "refactor", "files": ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"], "action": "modify"}], "files_likely_affected": ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"], "requires_core_change": false, "risk_level": "high", "risk_notes": "big change", "test_strategy": [], "estimated_complexity": "large", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Architect triage — XL (forces decomposition)
            final_response(
                r#"{"verdict": "proceed", "task_size": "XL", "sizing_rationale": "4+ files", "confidence": 0.9, "reasoning": "too large", "evidence": [], "architectural_notes": "", "suggested_changes": ["decompose"]}"#,
            ),
            // 3. Re-plan (triggered by XL) — but NO decomposition array
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "still too big", "files": ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"], "action": "modify"}], "files_likely_affected": ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"], "requires_core_change": false, "risk_level": "high", "risk_notes": "failed to decompose", "test_strategy": [], "estimated_complexity": "large", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
        ];

        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-xl-guard",
                "Large refactor that cannot be decomposed",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(
            result.status,
            PipelineStatus::PlanFailed,
            "XL task without decomposition should return PlanFailed, got {:?}, error: {:?}",
            result.status,
            result.error
        );
    }

    #[tokio::test]
    async fn pipeline_short_circuit_uses_task_size_s() {
        // Short-circuit tasks use TaskSize::M budget (65k + overhead tokens, 9 turns).
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let router = sequential_router_ref(short_circuit_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.pipeline.short_circuit_enabled = true;
        config.pipeline.short_circuit_max_files = 2;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router.clone(), config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-short-circuit-size",
                "Add a comment",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert!(
            result.status == PipelineStatus::Committed
                || result.status == PipelineStatus::PrCreated
                || result.status == PipelineStatus::PrMerged,
            "unexpected status: {:?}, error: {:?}",
            result.status,
            result.error
        );

        // Budget effective limit = M ceiling (120k) + planner overhead.
        // Short-circuit skips triage, so overhead is just the planner call.
        let summary = router.budget_summary().await;
        assert!(
            summary.tokens_remaining <= 121_000,
            "short-circuit should use ~M budget (120k + overhead), got remaining: {}",
            summary.tokens_remaining
        );
    }

    #[tokio::test]
    async fn pipeline_medium_3_files_strips_decomposition() {
        // Medium complexity with 3 files + decomposition → decomposition stripped,
        // proceeds to implementation (short-circuit eligible).
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner — medium complexity, 3 files, with decomposition
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "edit a"}, {"step_number": 2, "description": "edit b"}], "files_likely_affected": ["src/a.rs", "src/b.rs", "src/c.rs"], "requires_core_change": false, "risk_level": "medium", "risk_notes": "", "test_strategy": [], "estimated_complexity": "medium", "dependencies_affected": false, "public_api_changed": false, "decomposition": [{"id": "p1", "objective": "part 1"}, {"id": "p2", "objective": "part 2"}]}"#,
            ),
            // 2. Implementer — tool use (write_file)
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/a.rs", "content": "pub fn a() {}\n"}),
            }]),
            // 3. Implementer — final
            final_response(
                r#"{"files_changed": ["src/a.rs"], "tests_added": [], "commit_message": "feat: add a", "summary": "done"}"#,
            ),
        ];

        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.pipeline.short_circuit_enabled = true;
        config.pipeline.short_circuit_max_files = 3;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-med-3files",
                "Refactor across 3 files",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // Should NOT be decomposed — decomposition was stripped.
        assert_ne!(
            result.status,
            PipelineStatus::Decomposed,
            "medium task with ≤3 files should not be decomposed, got {:?}",
            result.status
        );
        // Plan should have decomposition removed.
        let plan = result.stage_outputs.get("plan").unwrap();
        assert!(
            plan.data.get("decomposition").is_none(),
            "decomposition should have been stripped from medium/3-file plan"
        );
    }

    #[tokio::test]
    async fn pipeline_medium_4_files_keeps_decomposition() {
        // Medium complexity with 4 files + decomposition → decomposition preserved.
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner — medium complexity, 4 files, with decomposition
            final_response(
                r#"{"steps": [], "files_likely_affected": ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"], "requires_core_change": false, "risk_level": "medium", "risk_notes": "", "test_strategy": [], "estimated_complexity": "medium", "dependencies_affected": false, "public_api_changed": false, "decomposition": [{"id": "p1", "objective": "part 1"}, {"id": "p2", "objective": "part 2"}]}"#,
            ),
        ];

        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-med-4files",
                "Refactor across 4 files",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(
            result.status,
            PipelineStatus::Decomposed,
            "medium task with 4 files should keep decomposition, got {:?}, error: {:?}",
            result.status,
            result.error
        );
    }

    #[tokio::test]
    async fn pipeline_implementation_failure_has_outcome_context() {
        // When the implementer returns an error, the pipeline should produce
        // an OutcomeContext with a ModelLimitation obstacle.
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner — trivial task (short-circuit eligible)
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "add feature", "files": ["src/new.rs"], "action": "create"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Implementer — returns stuck/parse_error with no escalation
            //    (mock router doesn't support escalation by default)
            final_response(
                r#"{"stuck": true, "stuck_reason": "stuck_loop", "files_changed": [], "summary": "got stuck"}"#,
            ),
        ];

        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.pipeline.short_circuit_enabled = true;
        config.pipeline.short_circuit_max_files = 2;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-impl-failure",
                "Add feature",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::ImplementationFailed);
        assert!(
            result.outcome_context.is_some(),
            "implementation failure should produce OutcomeContext"
        );
        let oc = result.outcome_context.unwrap();
        assert_eq!(oc.approach, "direct implementation");
        assert!(oc.recommendation.is_some());
        match &oc.obstacle {
            glitchlab_kernel::outcome::ObstacleKind::ModelLimitation { .. } => {}
            other => panic!("expected ModelLimitation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pipeline_boundary_violation_has_outcome_context() {
        // When the implementer hits a boundary violation, the pipeline should
        // produce an OutcomeContext with an ArchitecturalGap obstacle.
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner — trivial task (short-circuit eligible)
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "edit protected", "files": ["src/lib.rs"], "action": "modify"}], "files_likely_affected": ["src/lib.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
            ),
            // 2. Implementer — boundary violation
            final_response(
                r#"{"stuck": true, "stuck_reason": "boundary_violation", "files_changed": [], "summary": "hit protected path"}"#,
            ),
        ];

        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.pipeline.short_circuit_enabled = true;
        config.pipeline.short_circuit_max_files = 2;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        let result = pipeline
            .run(
                "test-boundary",
                "Edit protected file",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        assert_eq!(result.status, PipelineStatus::BoundaryViolation);
        assert!(
            result.outcome_context.is_some(),
            "boundary violation should produce OutcomeContext"
        );
        let oc = result.outcome_context.unwrap();
        match &oc.obstacle {
            glitchlab_kernel::outcome::ObstacleKind::ArchitecturalGap { description } => {
                assert!(
                    description.contains("protected path"),
                    "expected 'protected path' in description, got: {description}"
                );
            }
            other => panic!("expected ArchitecturalGap, got {other:?}"),
        }
        assert_eq!(
            oc.recommendation.as_deref(),
            Some("Decompose to exclude protected paths")
        );
    }

    // -----------------------------------------------------------------------
    // Leaf bead decomposition stripping
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn pipeline_leaf_bead_strips_decomposition() {
        // A task with a dot-notation ID (leaf bead) should have any planner
        // decomposition stripped, regardless of complexity.
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner — large complexity WITH decomposition
            final_response(
                r#"{"steps": [{"step_number": 1, "description": "impl"}], "files_likely_affected": ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"], "requires_core_change": false, "risk_level": "medium", "risk_notes": "", "test_strategy": [], "estimated_complexity": "large", "dependencies_affected": false, "public_api_changed": false, "decomposition": [{"id": "p1", "objective": "part 1"}, {"id": "p2", "objective": "part 2"}]}"#,
            ),
            // 2. Implementer — tool use (write_file)
            tool_response(vec![ToolCall {
                id: "toolu_01".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/a.rs", "content": "pub fn a() {}\n"}),
            }]),
            // 3. Implementer — final
            final_response(
                r#"{"files_changed": ["src/a.rs"], "tests_added": [], "commit_message": "feat: impl", "summary": "done"}"#,
            ),
        ];

        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;
        config.pipeline.short_circuit_enabled = true;
        config.pipeline.short_circuit_max_files = 5;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        // Leaf bead ID: "gl-abc.1" contains a dot at depth 0.
        let result = pipeline
            .run(
                "gl-abc.1",
                "Test tool-use loop with Ollama",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // Should NOT be decomposed — leaf bead decomposition was stripped.
        assert_ne!(
            result.status,
            PipelineStatus::Decomposed,
            "leaf bead should not be decomposed, got {:?}",
            result.status
        );
        // Plan should have decomposition removed.
        let plan = result.stage_outputs.get("plan").unwrap();
        assert!(
            plan.data.get("decomposition").is_none(),
            "decomposition should have been stripped from leaf bead plan"
        );
    }

    #[tokio::test]
    async fn pipeline_non_leaf_allows_decomposition() {
        // A task without dot notation (non-leaf) should preserve decomposition.
        let dir = tempfile::tempdir().unwrap();
        let base_branch = init_test_repo(dir.path());

        let responses = vec![
            // 1. Planner — large complexity with decomposition
            final_response(
                r#"{"steps": [], "files_likely_affected": ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "large", "dependencies_affected": false, "public_api_changed": false, "decomposition": [{"id": "sub-1", "description": "sub-task 1"}, {"id": "sub-2", "description": "sub-task 2"}]}"#,
            ),
        ];
        let router = sequential_router_ref(responses);
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let history: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let pipeline =
            EngineeringPipeline::new(router, config, handler, history, default_mock_ops());

        // Non-leaf ID: "gl-abc" has no dot.
        let result = pipeline
            .run(
                "gl-abc",
                "Build a complex feature",
                dir.path(),
                &base_branch,
                &[],
            )
            .await;

        // Decomposition should be preserved — large task, not a leaf bead.
        assert_eq!(
            result.status,
            PipelineStatus::Decomposed,
            "non-leaf large task should keep decomposition, got {:?}",
            result.status
        );
    }
}
