use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use chrono::Utc;
use glitchlab_kernel::agent::{Agent, AgentContext, AgentMetadata, AgentOutput};
use glitchlab_kernel::governance::BoundaryEnforcer;
use glitchlab_kernel::pipeline::{
    EventKind, PipelineContext, PipelineEvent, PipelineResult, PipelineStatus,
};
use glitchlab_memory::history::{BudgetSnapshot, EventsSummary, HistoryEntry, TaskHistory};
use tokio::process::Command;
use tracing::{info, warn};

use crate::agents::RouterRef;
use crate::agents::archivist::ArchivistAgent;
use crate::agents::debugger::DebuggerAgent;
use crate::agents::implementer::ImplementerAgent;
use crate::agents::planner::PlannerAgent;
use crate::agents::release::ReleaseAgent;
use crate::agents::security::SecurityAgent;
use crate::config::EngConfig;
use crate::indexer;
use crate::workspace::Workspace;

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
// EngineeringPipeline
// ---------------------------------------------------------------------------

/// Full engineering pipeline:
/// Plan -> Implement -> Test/Debug -> Security -> Release -> Archive -> Commit -> PR.
pub struct EngineeringPipeline {
    router: RouterRef,
    config: EngConfig,
    handler: Arc<dyn InterventionHandler>,
}

impl EngineeringPipeline {
    pub fn new(
        router: RouterRef,
        config: EngConfig,
        handler: Arc<dyn InterventionHandler>,
    ) -> Self {
        Self {
            router,
            config,
            handler,
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
    ) -> PipelineResult {
        info!(task_id, "pipeline starting");

        let mut workspace = Workspace::new(repo_path, task_id, &self.config.workspace.worktree_dir);

        let result = self
            .run_stages(task_id, objective, repo_path, base_branch, &mut workspace)
            .await;

        // Always cleanup workspace.
        if let Err(e) = workspace.cleanup().await {
            warn!(task_id, error = %e, "workspace cleanup failed");
        }

        // Record history.
        let history = TaskHistory::new(repo_path);
        let entry = build_history_entry(task_id, &result);
        if let Err(e) = history.record(&entry) {
            warn!(task_id, error = %e, "failed to record history");
        }

        info!(task_id, status = ?result.status, "pipeline complete");
        result
    }

    async fn run_stages(
        &self,
        task_id: &str,
        objective: &str,
        repo_path: &Path,
        base_branch: &str,
        workspace: &mut Workspace,
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
        let repo_context = match indexer::build_index(repo_path).await {
            Ok(index) => index.to_agent_context(100),
            Err(e) => {
                warn!(task_id, error = %e, "indexer failed, continuing");
                String::new()
            }
        };

        let failure_context = TaskHistory::new(repo_path)
            .build_failure_context(5)
            .unwrap_or_default();

        let mut enriched = format!("## Task\n\n{objective}");
        if !repo_context.is_empty() {
            enriched.push_str("\n\n");
            enriched.push_str(&repo_context);
        }
        if !failure_context.is_empty() {
            enriched.push_str("\n\n");
            enriched.push_str(&failure_context);
        }
        ctx.agent_context.objective = enriched;

        // --- Stage 3: Plan ---
        ctx.current_stage = Some("plan".into());
        let planner = PlannerAgent::new(Arc::clone(&self.router));
        let plan_output = match planner.execute(&ctx.agent_context).await {
            Ok(o) => o,
            Err(e) => {
                return self
                    .fail(ctx, PipelineStatus::PlanFailed, e.to_string())
                    .await;
            }
        };
        self.emit(&mut ctx, EventKind::PlanCreated, plan_output.data.clone());
        ctx.stage_outputs.insert("plan".into(), plan_output.clone());

        // --- Stage 4: Boundary check ---
        ctx.current_stage = Some("boundary_check".into());
        let files_affected: Vec<String> = plan_output.data["files_likely_affected"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let requires_core = plan_output.data["requires_core_change"]
            .as_bool()
            .unwrap_or(false);

        let enforcer = BoundaryEnforcer::new(
            self.config
                .boundaries
                .protected_paths
                .iter()
                .map(PathBuf::from)
                .collect(),
        );

        if let Err(e) = enforcer.enforce(&files_affected, requires_core) {
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
        let implementer = ImplementerAgent::new(Arc::clone(&self.router));
        let impl_output = match implementer.execute(&ctx.agent_context).await {
            Ok(o) => o,
            Err(e) => {
                return self
                    .fail(ctx, PipelineStatus::ImplementationFailed, e.to_string())
                    .await;
            }
        };
        self.emit(
            &mut ctx,
            EventKind::ImplementationComplete,
            impl_output.data.clone(),
        );
        ctx.stage_outputs
            .insert("implement".into(), impl_output.clone());

        // --- Stage 7: Apply changes ---
        let changes = extract_changes(&impl_output.data);
        if let Err(e) = apply_changes(&wt_path, &changes).await {
            return self
                .fail(
                    ctx,
                    PipelineStatus::ImplementationFailed,
                    format!("apply changes failed: {e}"),
                )
                .await;
        }

        // --- Stage 8-9: Test / debug loop ---
        ctx.current_stage = Some("test".into());
        let test_cmd = crate::config::detect_test_command(repo_path);
        let mut fix_attempts = 0u32;
        let max_fixes = self.config.limits.max_fix_attempts;

        if let Some(ref cmd) = test_cmd {
            loop {
                match run_tests(cmd, &wt_path).await {
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
                            return self
                                .fail(
                                    ctx,
                                    PipelineStatus::TestsFailed,
                                    format!("tests failing after {max_fixes} fix attempts"),
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

                        let debugger = DebuggerAgent::new(Arc::clone(&self.router));
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

                        // Apply debug fix.
                        let fix_changes = debug_out
                            .data
                            .get("fix")
                            .map(extract_changes)
                            .unwrap_or_default();
                        if let Err(e) = apply_changes(&wt_path, &fix_changes).await {
                            return self
                                .fail(
                                    ctx,
                                    PipelineStatus::TestsFailed,
                                    format!("apply debug fix failed: {e}"),
                                )
                                .await;
                        }

                        ctx.current_stage = Some("test".into());
                    }
                }
            }
        }

        // --- Stage 10: Security review ---
        ctx.current_stage = Some("security".into());
        let diff = workspace.diff_full(base_branch).await.unwrap_or_default();
        ctx.agent_context.previous_output = serde_json::json!({
            "diff": truncate(&diff, 4000),
            "plan": plan_output.data,
            "implementation": impl_output.data,
        });

        let security_agent = SecurityAgent::new(Arc::clone(&self.router));
        let security_output = match security_agent.execute(&ctx.agent_context).await {
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

        let verdict = security_output.data["verdict"].as_str().unwrap_or("pass");
        self.emit(
            &mut ctx,
            EventKind::SecurityReview,
            security_output.data.clone(),
        );
        ctx.stage_outputs
            .insert("security".into(), security_output.clone());

        if verdict == "block" {
            return self
                .fail(
                    ctx,
                    PipelineStatus::SecurityBlocked,
                    "security review blocked changes".into(),
                )
                .await;
        }

        // --- Stage 11: Release assessment ---
        ctx.current_stage = Some("release".into());
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

        // --- Stage 12: Archive / documentation ---
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

        // --- Stage 13: Commit ---
        ctx.current_stage = Some("commit".into());
        let commit_msg = impl_output.data["commit_message"]
            .as_str()
            .unwrap_or("chore: automated changes by GLITCHLAB");

        let commit_sha = match workspace.commit(commit_msg).await {
            Ok(sha) => sha,
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

        // --- Stage 14: Human gate — PR review ---
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
                };
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
            };
        }

        let pr_url = match create_pr(
            &wt_path,
            workspace.branch_name(),
            base_branch,
            objective,
            &plan_output,
            &security_output,
        )
        .await
        {
            Ok(url) => url,
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
                };
            }
        };

        self.emit(
            &mut ctx,
            EventKind::PrCreated,
            serde_json::json!({ "url": &pr_url }),
        );

        let budget = self.router.budget_summary().await;
        PipelineResult {
            status: PipelineStatus::PrCreated,
            stage_outputs: ctx.stage_outputs,
            events: ctx.events,
            budget,
            pr_url: Some(pr_url),
            branch: Some(workspace.branch_name().into()),
            error: None,
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
        let budget = self.router.budget_summary().await;
        PipelineResult {
            status,
            stage_outputs: ctx.stage_outputs,
            events: ctx.events,
            budget,
            pr_url: None,
            branch: None,
            error: Some(error),
        }
    }
}

// ---------------------------------------------------------------------------
// Change application
// ---------------------------------------------------------------------------

/// Extract the changes array from agent output data.
/// Handles both implementer format (`changes` + `tests_added`) and
/// debugger format (`changes` only).
fn extract_changes(data: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut changes = Vec::new();

    if let Some(arr) = data["changes"].as_array() {
        changes.extend(arr.iter().cloned());
    }

    // Implementer also produces tests_added — treat as creates.
    if let Some(arr) = data["tests_added"].as_array() {
        for test in arr {
            changes.push(serde_json::json!({
                "file": test["file"],
                "action": "create",
                "content": test["content"],
                "description": test["description"],
            }));
        }
    }

    changes
}

/// Apply file changes to the worktree.
async fn apply_changes(worktree: &Path, changes: &[serde_json::Value]) -> Result<(), String> {
    for change in changes {
        let file = change["file"]
            .as_str()
            .ok_or_else(|| "change missing 'file' field".to_string())?;
        let action = change["action"].as_str().unwrap_or("modify");
        let full_path = worktree.join(file);

        match action {
            "create" => {
                let content = change["content"].as_str().unwrap_or("");
                if let Some(parent) = full_path.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
                }
                tokio::fs::write(&full_path, content)
                    .await
                    .map_err(|e| format!("write {file}: {e}"))?;
            }
            "delete" => {
                if full_path.exists() {
                    tokio::fs::remove_file(&full_path)
                        .await
                        .map_err(|e| format!("delete {file}: {e}"))?;
                }
            }
            _ => {
                // modify or any unknown action: try patch, fall back to content.
                if let Some(patch) = change["patch"].as_str()
                    && !patch.is_empty()
                    && try_git_apply(worktree, patch).await.is_ok()
                {
                    continue;
                }
                if let Some(content) = change["content"].as_str() {
                    if let Some(parent) = full_path.parent() {
                        tokio::fs::create_dir_all(parent)
                            .await
                            .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
                    }
                    tokio::fs::write(&full_path, content)
                        .await
                        .map_err(|e| format!("write {file}: {e}"))?;
                }
            }
        }
    }
    Ok(())
}

/// Try to apply a unified diff via `git apply`.
async fn try_git_apply(worktree: &Path, patch: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let mut child = Command::new("git")
        .args(["-C", &worktree.to_string_lossy(), "apply", "--3way", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn git apply: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(patch.as_bytes())
            .await
            .map_err(|e| format!("write patch: {e}"))?;
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("wait git apply: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("git apply failed: {stderr}"))
    }
}

// ---------------------------------------------------------------------------
// Test runner
// ---------------------------------------------------------------------------

/// Run the project's test command in the worktree.
/// Returns Ok(()) on success, Err(combined output) on failure.
async fn run_tests(cmd: &str, worktree: &Path) -> Result<(), String> {
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
}

// ---------------------------------------------------------------------------
// PR creation
// ---------------------------------------------------------------------------

/// Create a pull request via `gh pr create`.
async fn create_pr(
    worktree: &Path,
    branch: &str,
    base: &str,
    title: &str,
    plan: &AgentOutput,
    security: &AgentOutput,
) -> Result<String, String> {
    let body = build_pr_body(plan, security);

    let output = Command::new("gh")
        .args([
            "pr",
            "create",
            "--title",
            &truncate(title, 70),
            "--body",
            &body,
            "--base",
            base,
            "--head",
            branch,
        ])
        .current_dir(worktree)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("gh pr create: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("gh pr create failed: {stderr}"))
    }
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

fn truncate(s: &str, max: usize) -> String {
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
        timestamp: Utc::now().to_rfc3339(),
        task_id: task_id.into(),
        status,
        pr_url: result.pr_url.clone(),
        branch: result.branch.clone(),
        error: result.error.clone(),
        budget: BudgetSnapshot {
            total_tokens: result.budget.total_tokens,
            estimated_cost: result.budget.estimated_cost,
            call_count: result.budget.call_count,
            tokens_remaining: result.budget.tokens_remaining,
            dollars_remaining: result.budget.dollars_remaining,
        },
        events_summary: build_events_summary(&result.stage_outputs),
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
    use crate::agents::test_helpers::mock_router_ref;
    use glitchlab_kernel::agent::{AgentMetadata, AgentOutput};
    use glitchlab_kernel::budget::BudgetSummary;

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
        let router = mock_router_ref();
        let config = EngConfig::default();
        let handler = Arc::new(AutoApproveHandler);
        let _pipeline = EngineeringPipeline::new(router, config, handler);
    }

    #[test]
    fn extract_changes_from_implementer() {
        let data = serde_json::json!({
            "changes": [
                {"file": "src/lib.rs", "action": "modify", "content": "code"},
            ],
            "tests_added": [
                {"file": "tests/test.rs", "content": "test code", "description": "unit tests"},
            ],
        });
        let changes = extract_changes(&data);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0]["file"], "src/lib.rs");
        assert_eq!(changes[1]["action"], "create");
    }

    #[test]
    fn extract_changes_from_debugger() {
        let data = serde_json::json!({
            "changes": [
                {"file": "src/bug.rs", "action": "modify", "patch": "diff"},
            ],
        });
        let changes = extract_changes(&data);
        assert_eq!(changes.len(), 1);
    }

    #[test]
    fn extract_changes_empty() {
        let data = serde_json::json!({});
        let changes = extract_changes(&data);
        assert!(changes.is_empty());
    }

    #[tokio::test]
    async fn apply_changes_create_file() {
        let dir = tempfile::tempdir().unwrap();
        let changes = vec![serde_json::json!({
            "file": "src/new.rs",
            "action": "create",
            "content": "fn main() {}",
        })];
        apply_changes(dir.path(), &changes).await.unwrap();
        let content = tokio::fs::read_to_string(dir.path().join("src/new.rs"))
            .await
            .unwrap();
        assert_eq!(content, "fn main() {}");
    }

    #[tokio::test]
    async fn apply_changes_delete_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("to_delete.txt");
        tokio::fs::write(&file_path, "goodbye").await.unwrap();
        assert!(file_path.exists());

        let changes = vec![serde_json::json!({
            "file": "to_delete.txt",
            "action": "delete",
        })];
        apply_changes(dir.path(), &changes).await.unwrap();
        assert!(!file_path.exists());
    }

    #[tokio::test]
    async fn apply_changes_delete_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let changes = vec![serde_json::json!({
            "file": "nonexistent.txt",
            "action": "delete",
        })];
        // Should not error when file doesn't exist.
        apply_changes(dir.path(), &changes).await.unwrap();
    }

    #[tokio::test]
    async fn apply_changes_modify_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("existing.rs");
        tokio::fs::write(&file_path, "old content").await.unwrap();

        let changes = vec![serde_json::json!({
            "file": "existing.rs",
            "action": "modify",
            "content": "new content",
        })];
        apply_changes(dir.path(), &changes).await.unwrap();
        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "new content");
    }

    #[tokio::test]
    async fn apply_changes_missing_file_field() {
        let dir = tempfile::tempdir().unwrap();
        let changes = vec![serde_json::json!({
            "action": "create",
            "content": "code",
        })];
        let result = apply_changes(dir.path(), &changes).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing 'file' field"));
    }

    #[tokio::test]
    async fn apply_changes_empty() {
        let dir = tempfile::tempdir().unwrap();
        apply_changes(dir.path(), &[]).await.unwrap();
    }

    #[tokio::test]
    async fn run_tests_success() {
        let dir = tempfile::tempdir().unwrap();
        // `true` always succeeds on Unix.
        run_tests("true", dir.path()).await.unwrap();
    }

    #[tokio::test]
    async fn run_tests_failure() {
        let dir = tempfile::tempdir().unwrap();
        // `false` always fails on Unix.
        let result = run_tests("false", dir.path()).await;
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
    fn truncate_long_string() {
        let result = truncate("hello world", 5);
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
        };

        let entry = build_history_entry("test-1", &result);
        assert_eq!(entry.task_id, "test-1");
        assert_eq!(entry.status, "pr_created");
        assert!(entry.pr_url.is_some());
        assert!(entry.branch.is_some());
        assert!(entry.error.is_none());
        assert_eq!(entry.budget.total_tokens, 500);
        assert_eq!(entry.budget.call_count, 3);
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

        let router = mock_router_ref();
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let pipeline = EngineeringPipeline::new(router, config, handler);

        let result = pipeline
            .run("test-task-1", "Fix a bug", dir.path(), &base_branch)
            .await;

        // Pipeline runs through all stages. Push fails (no remote),
        // so final status is Committed.
        assert!(
            result.status == PipelineStatus::Committed
                || result.status == PipelineStatus::PrCreated,
            "unexpected status: {:?}",
            result.status
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
        let pipeline = EngineeringPipeline::new(router, config, handler);

        let result = pipeline
            .run("test-reject", "Fix something", dir.path(), &base_branch)
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
    async fn try_git_apply_valid_patch() {
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        // Create a tracked file.
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "f.txt"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add f"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let patch = "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        try_git_apply(dir.path(), patch).await.unwrap();

        let content = tokio::fs::read_to_string(dir.path().join("f.txt"))
            .await
            .unwrap();
        assert!(content.contains("B"));
    }

    #[tokio::test]
    async fn try_git_apply_invalid_patch() {
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        let result = try_git_apply(dir.path(), "not a valid patch").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("git apply failed"));
    }

    #[tokio::test]
    async fn apply_changes_modify_with_patch() {
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        std::fs::write(dir.path().join("src.rs"), "fn old() {}\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "src.rs"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add src"])
            .current_dir(dir.path())
            .output()
            .unwrap();

        let patch = "--- a/src.rs\n+++ b/src.rs\n@@ -1 +1 @@\n-fn old() {}\n+fn new_func() {}\n";
        let changes = vec![serde_json::json!({
            "file": "src.rs",
            "action": "modify",
            "patch": patch,
        })];

        apply_changes(dir.path(), &changes).await.unwrap();
        let content = tokio::fs::read_to_string(dir.path().join("src.rs"))
            .await
            .unwrap();
        assert!(content.contains("new_func"));
    }

    #[tokio::test]
    async fn run_tests_empty_command() {
        let dir = tempfile::tempdir().unwrap();
        run_tests("", dir.path()).await.unwrap();
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

        let router = mock_router_ref();
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let pipeline = EngineeringPipeline::new(router, config, handler);

        let result = pipeline
            .run("test-with-tests", "Fix a bug", dir.path(), &base_branch)
            .await;

        // Tests should pass, then push fails (no remote) → Committed.
        assert!(
            result.status == PipelineStatus::Committed
                || result.status == PipelineStatus::PrCreated,
            "unexpected status: {:?}",
            result.status
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

        let router = mock_router_ref();
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = true;
        config.intervention.pause_before_pr = true;

        let handler = Arc::new(PrRejectHandler);
        let pipeline = EngineeringPipeline::new(router, config, handler);

        let result = pipeline
            .run("test-pr-reject", "Fix something", dir.path(), &base_branch)
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

        let router = mock_router_ref();
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let pipeline = EngineeringPipeline::new(router, config, handler);

        let result = pipeline
            .run("test-fail-debug", "Fix a bug", dir.path(), &base_branch)
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
    async fn create_pr_without_gh() {
        let dir = tempfile::tempdir().unwrap();
        let plan = fallback_output(
            "planner",
            serde_json::json!({
                "steps": [{"description": "step 1"}],
                "risk_level": "low"
            }),
        );
        let security = fallback_output("security", serde_json::json!({"verdict": "pass"}));

        // gh pr create will fail — no git repo, no remote.
        let result = create_pr(
            dir.path(),
            "test-branch",
            "main",
            "Test PR",
            &plan,
            &security,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn run_tests_multiword_command() {
        let dir = tempfile::tempdir().unwrap();
        // Multi-word command that succeeds.
        run_tests("echo hello world", dir.path()).await.unwrap();
    }
}
