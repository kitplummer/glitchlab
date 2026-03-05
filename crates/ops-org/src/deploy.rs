use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use glitchlab_kernel::agent::{Agent, AgentContext};
use glitchlab_kernel::pipeline::PipelineStatus;

use crate::agents::RouterRef;
use crate::agents::uplink::UplinkSreAgent;
use crate::config::{DeployTarget, OpsConfig};
use crate::fly::{FlyExecutor, FlyOutput};
use crate::smoke::{HttpClient, SmokeTestReport, run_smoke_tests};

// ---------------------------------------------------------------------------
// DeployOptions
// ---------------------------------------------------------------------------

/// Options for a deploy pipeline run.
#[derive(Debug, Clone)]
pub struct DeployOptions {
    pub app_name: String,
    pub auto_approve: bool,
    pub dry_run: bool,
    pub working_dir: PathBuf,
    pub smoke_timeout: Duration,
}

impl Default for DeployOptions {
    fn default() -> Self {
        Self {
            app_name: "lowendinsight".into(),
            auto_approve: false,
            dry_run: false,
            working_dir: PathBuf::from("."),
            smoke_timeout: Duration::from_secs(10),
        }
    }
}

// ---------------------------------------------------------------------------
// PreCheckOutcome — classify flyctl status result
// ---------------------------------------------------------------------------

/// Classification of a pre-check result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreCheckOutcome {
    /// App exists and is reachable.
    AppExists,
    /// App was not found (candidate for auto-create).
    AppNotFound,
    /// Some other error (auth, network, etc.).
    OtherError(String),
}

/// Classify a `flyctl status` output into a `PreCheckOutcome`.
pub fn classify_precheck(output: &FlyOutput) -> PreCheckOutcome {
    if output.success {
        return PreCheckOutcome::AppExists;
    }
    let stderr_lower = output.stderr.to_lowercase();
    if stderr_lower.contains("could not find app") || stderr_lower.contains("app not found") {
        PreCheckOutcome::AppNotFound
    } else {
        PreCheckOutcome::OtherError(output.stderr.trim().to_string())
    }
}

// ---------------------------------------------------------------------------
// DeployStatus
// ---------------------------------------------------------------------------

/// Outcome status of a deploy pipeline run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeployStatus {
    Healthy,
    Degraded,
    RolledBack,
    DeployFailed,
    Escalated,
    PreCheckFailed,
    Rejected,
    DryRun,
    RollbackFailed,
}

impl std::fmt::Display for DeployStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeployStatus::Healthy => write!(f, "healthy"),
            DeployStatus::Degraded => write!(f, "degraded"),
            DeployStatus::RolledBack => write!(f, "rolled_back"),
            DeployStatus::DeployFailed => write!(f, "deploy_failed"),
            DeployStatus::Escalated => write!(f, "escalated"),
            DeployStatus::PreCheckFailed => write!(f, "pre_check_failed"),
            DeployStatus::Rejected => write!(f, "rejected"),
            DeployStatus::DryRun => write!(f, "dry_run"),
            DeployStatus::RollbackFailed => write!(f, "rollback_failed"),
        }
    }
}

// ---------------------------------------------------------------------------
// DeployResult
// ---------------------------------------------------------------------------

/// Full result of a deploy pipeline run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployResult {
    pub status: DeployStatus,
    pub stages_completed: Vec<String>,
    pub smoke_report: Option<SmokeTestReport>,
    pub assessment: Option<serde_json::Value>,
    pub error: Option<String>,
    pub budget_cost: f64,
}

impl DeployResult {
    /// Convert to a `PipelineResult` for orchestrator outcome routing.
    pub fn to_pipeline_result(&self) -> glitchlab_kernel::pipeline::PipelineResult {
        let status = match self.status {
            DeployStatus::Healthy | DeployStatus::DryRun | DeployStatus::Degraded => {
                PipelineStatus::Committed
            }
            DeployStatus::RolledBack
            | DeployStatus::DeployFailed
            | DeployStatus::PreCheckFailed
            | DeployStatus::RollbackFailed => PipelineStatus::ImplementationFailed,
            DeployStatus::Rejected | DeployStatus::Escalated => PipelineStatus::Escalated,
        };
        glitchlab_kernel::pipeline::PipelineResult {
            status,
            stage_outputs: HashMap::new(),
            events: vec![],
            budget: glitchlab_kernel::budget::BudgetSummary {
                total_tokens: 0,
                estimated_cost: self.budget_cost,
                call_count: 0,
                tokens_remaining: 0,
                dollars_remaining: 0.0,
            },
            pr_url: None,
            branch: None,
            error: self.error.clone(),
            outcome_context: None,
        }
    }
}

// ---------------------------------------------------------------------------
// DeployApprovalHandler — governance gate
// ---------------------------------------------------------------------------

/// Handle deploy approval gates (CLI prompt, Slack, etc.).
pub trait DeployApprovalHandler: Send + Sync {
    fn request_deploy_approval<'a>(
        &'a self,
        app: &'a str,
        summary: &'a str,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}

/// Auto-approves all deploys (for CI/autonomous mode).
pub struct AutoApproveDeployHandler;

impl DeployApprovalHandler for AutoApproveDeployHandler {
    fn request_deploy_approval<'a>(
        &'a self,
        _app: &'a str,
        _summary: &'a str,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async { true })
    }
}

// ---------------------------------------------------------------------------
// DeployPipeline
// ---------------------------------------------------------------------------

/// The deploy pipeline orchestrator.
///
/// Chains: PreCheck → Deploy → SmokeTest → Assessment → Act.
pub struct DeployPipeline {
    pub config: OpsConfig,
    pub target: DeployTarget,
    pub fly: Arc<dyn FlyExecutor>,
    pub http: Arc<dyn HttpClient>,
    pub router: RouterRef,
    pub approval: Arc<dyn DeployApprovalHandler>,
}

impl DeployPipeline {
    /// Execute the full deploy pipeline.
    pub async fn run(&self, opts: &DeployOptions) -> DeployResult {
        let mut stages: Vec<String> = Vec::new();
        let mut budget_cost = 0.0;

        // Stage 1: PreCheck — verify the app is reachable
        info!(app = %opts.app_name, "deploy pipeline: pre-check");
        let status_output = self.fly.status(&opts.app_name).await;
        let precheck = classify_precheck(&status_output);

        match precheck {
            PreCheckOutcome::AppExists => {
                stages.push("pre_check".into());
            }
            PreCheckOutcome::AppNotFound => {
                if opts.dry_run {
                    warn!(app = %opts.app_name, "app not found — dry-run, continuing to smoke tests");
                    stages.push("pre_check_warn".into());
                } else {
                    // Auto-create via governance gate
                    info!(app = %opts.app_name, "app not found — requesting create approval");
                    let summary = format!("App '{}' does not exist. Create it?", opts.app_name);
                    let approved = if self.config.governance_settings.require_deploy_approval
                        && !opts.auto_approve
                    {
                        self.approval
                            .request_deploy_approval(&opts.app_name, &summary)
                            .await
                    } else {
                        true
                    };

                    if !approved {
                        return DeployResult {
                            status: DeployStatus::Rejected,
                            stages_completed: stages,
                            smoke_report: None,
                            assessment: None,
                            error: Some("app creation rejected by governance gate".into()),
                            budget_cost,
                        };
                    }

                    let create_output = self
                        .fly
                        .create(&opts.app_name, self.target.fly_org.as_deref())
                        .await;
                    if !create_output.success {
                        return DeployResult {
                            status: DeployStatus::PreCheckFailed,
                            stages_completed: stages,
                            smoke_report: None,
                            assessment: None,
                            error: Some(format!(
                                "app creation failed: {}",
                                create_output.stderr.trim()
                            )),
                            budget_cost,
                        };
                    }
                    info!(app = %opts.app_name, "app created successfully");
                    stages.push("app_created".into());
                    stages.push("pre_check".into());
                }
            }
            PreCheckOutcome::OtherError(ref msg) => {
                if opts.dry_run {
                    warn!(app = %opts.app_name, error = %msg, "pre-check error — dry-run, continuing to smoke tests");
                    stages.push("pre_check_warn".into());
                } else {
                    return DeployResult {
                        status: DeployStatus::PreCheckFailed,
                        stages_completed: stages,
                        smoke_report: None,
                        assessment: None,
                        error: Some(format!("pre-check failed: {msg}")),
                        budget_cost,
                    };
                }
            }
        }

        // Stage 2: Dry-run — run smoke tests for informational purposes only
        if opts.dry_run {
            info!(app = %opts.app_name, "deploy pipeline: dry run — running smoke tests only");
            let smoke_report =
                run_smoke_tests(&self.target, self.http.as_ref(), opts.smoke_timeout).await;
            stages.push("dry_run_smoke".into());

            return DeployResult {
                status: DeployStatus::DryRun,
                stages_completed: stages,
                smoke_report: Some(smoke_report),
                assessment: None,
                error: None,
                budget_cost,
            };
        }

        // Stage 3: Governance gate
        if self.config.governance_settings.require_deploy_approval && !opts.auto_approve {
            let summary = format!(
                "Deploy {} from {}",
                opts.app_name,
                opts.working_dir.display()
            );
            let approved = self
                .approval
                .request_deploy_approval(&opts.app_name, &summary)
                .await;
            if !approved {
                return DeployResult {
                    status: DeployStatus::Rejected,
                    stages_completed: stages,
                    smoke_report: None,
                    assessment: None,
                    error: Some("deploy rejected by governance gate".into()),
                    budget_cost,
                };
            }
        }
        stages.push("governance".into());

        // Stage 4: Deploy
        info!(app = %opts.app_name, "deploy pipeline: deploying");
        let config_path = self.target.fly_toml_path.as_ref().map(PathBuf::from);
        let deploy_output = self
            .fly
            .deploy(&opts.app_name, &opts.working_dir, config_path.as_deref())
            .await;
        if !deploy_output.success {
            return DeployResult {
                status: DeployStatus::DeployFailed,
                stages_completed: stages,
                smoke_report: None,
                assessment: None,
                error: Some(format!("deploy failed: {}", deploy_output.stderr.trim())),
                budget_cost,
            };
        }
        stages.push("deploy".into());

        // Stage 5: Smoke tests
        info!(app = %opts.app_name, "deploy pipeline: running smoke tests");
        let smoke_report =
            run_smoke_tests(&self.target, self.http.as_ref(), opts.smoke_timeout).await;
        stages.push("smoke_test".into());

        // Stage 6: Uplink assessment
        info!(app = %opts.app_name, "deploy pipeline: Uplink assessment");
        let deploy_context = format!(
            "app: {}, working_dir: {}, deploy_stdout: {}",
            opts.app_name,
            opts.working_dir.display(),
            deploy_output.stdout.chars().take(500).collect::<String>(),
        );
        let health_text = smoke_report.as_health_results_text();

        let mut extra = HashMap::new();
        extra.insert(
            "deploy_context".into(),
            serde_json::Value::String(deploy_context),
        );
        extra.insert(
            "health_results".into(),
            serde_json::Value::String(health_text),
        );

        let ctx = AgentContext {
            task_id: format!("deploy-{}", opts.app_name),
            objective: format!(
                "Assess post-deploy health for app '{}'. Determine if the deploy is healthy, \
                 degraded, or requires rollback.",
                opts.app_name
            ),
            repo_path: opts.working_dir.to_string_lossy().into(),
            working_dir: opts.working_dir.to_string_lossy().into(),
            constraints: vec![],
            acceptance_criteria: vec![],
            risk_level: "medium".into(),
            file_context: HashMap::new(),
            previous_output: serde_json::Value::Null,
            extra,
        };

        let uplink = UplinkSreAgent::new(self.router.clone());
        let assessment = match uplink.execute(&ctx).await {
            Ok(output) => {
                budget_cost = output.metadata.cost;
                output.data
            }
            Err(e) => {
                warn!(error = %e, "Uplink assessment failed, escalating");
                stages.push("assessment_failed".into());
                return DeployResult {
                    status: DeployStatus::Escalated,
                    stages_completed: stages,
                    smoke_report: Some(smoke_report),
                    assessment: None,
                    error: Some(format!("uplink assessment error: {e}")),
                    budget_cost,
                };
            }
        };
        stages.push("assessment".into());

        // Stage 7: Act on recommendation
        let recommendation = assessment["recommendation"].as_str().unwrap_or("escalate");

        let status = match recommendation {
            "none" => DeployStatus::Healthy,
            "investigate" => DeployStatus::Degraded,
            "rollback" => {
                info!(app = %opts.app_name, "deploy pipeline: executing rollback");
                let rollback_output = self.fly.rollback(&opts.app_name).await;
                if rollback_output.success {
                    DeployStatus::RolledBack
                } else {
                    warn!(
                        app = %opts.app_name,
                        stderr = %rollback_output.stderr,
                        "rollback failed"
                    );
                    DeployStatus::RollbackFailed
                }
            }
            _ => DeployStatus::Escalated,
        };
        stages.push("act".into());

        DeployResult {
            status,
            stages_completed: stages,
            smoke_report: Some(smoke_report),
            assessment: Some(assessment),
            error: None,
            budget_cost,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::test_helpers::{final_response, mock_router_ref, sequential_router_ref};
    use crate::config::DeployTarget;
    use crate::fly::FlyOutput;
    use crate::smoke::HttpResponse;
    use std::sync::Mutex;

    // --- Mock FlyExecutor ---

    struct MockFlyExecutor {
        status_output: FlyOutput,
        deploy_output: FlyOutput,
        rollback_output: FlyOutput,
        create_output: FlyOutput,
    }

    impl FlyExecutor for MockFlyExecutor {
        fn status<'a>(
            &'a self,
            _app: &'a str,
        ) -> Pin<Box<dyn Future<Output = FlyOutput> + Send + 'a>> {
            let output = self.status_output.clone();
            Box::pin(async move { output })
        }

        fn deploy<'a>(
            &'a self,
            _app: &'a str,
            _working_dir: &'a std::path::Path,
            _config_path: Option<&'a std::path::Path>,
        ) -> Pin<Box<dyn Future<Output = FlyOutput> + Send + 'a>> {
            let output = self.deploy_output.clone();
            Box::pin(async move { output })
        }

        fn rollback<'a>(
            &'a self,
            _app: &'a str,
        ) -> Pin<Box<dyn Future<Output = FlyOutput> + Send + 'a>> {
            let output = self.rollback_output.clone();
            Box::pin(async move { output })
        }

        fn create<'a>(
            &'a self,
            _app: &'a str,
            _org: Option<&'a str>,
        ) -> Pin<Box<dyn Future<Output = FlyOutput> + Send + 'a>> {
            let output = self.create_output.clone();
            Box::pin(async move { output })
        }
    }

    fn success_fly() -> FlyOutput {
        FlyOutput {
            success: true,
            stdout: "ok".into(),
            stderr: String::new(),
            exit_code: 0,
        }
    }

    fn failure_fly(msg: &str) -> FlyOutput {
        FlyOutput {
            success: false,
            stdout: String::new(),
            stderr: msg.into(),
            exit_code: 1,
        }
    }

    // --- Mock HttpClient ---

    struct MockDeployHttpClient {
        responses: Mutex<Vec<HttpResponse>>,
    }

    impl MockDeployHttpClient {
        fn all_ok(count: usize) -> Self {
            Self {
                responses: Mutex::new(vec![
                    HttpResponse {
                        status: 200,
                        body: "LowEndInsight".into(),
                        latency_ms: 10,
                        error: None,
                    };
                    count
                ]),
            }
        }
    }

    impl HttpClient for MockDeployHttpClient {
        fn request<'a>(
            &'a self,
            _method: &'a str,
            _url: &'a str,
            _timeout: Duration,
        ) -> Pin<Box<dyn Future<Output = HttpResponse> + Send + 'a>> {
            let resp = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(HttpResponse {
                    status: 200,
                    body: "ok".into(),
                    latency_ms: 0,
                    error: None,
                });
            Box::pin(async move { resp })
        }
    }

    // --- Mock RejectHandler ---

    struct RejectDeployHandler;

    impl DeployApprovalHandler for RejectDeployHandler {
        fn request_deploy_approval<'a>(
            &'a self,
            _app: &'a str,
            _summary: &'a str,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            Box::pin(async { false })
        }
    }

    fn test_target() -> DeployTarget {
        DeployTarget {
            app_name: "test-app".into(),
            base_url: "https://test.fly.dev".into(),
            smoke_tests: vec![crate::config::SmokeTestEndpoint {
                method: "GET".into(),
                path: "/".into(),
                expected_status: 200,
                expected_body_contains: None,
                required: true,
            }],
            fly_toml_path: None,
            fly_org: None,
        }
    }

    fn test_opts() -> DeployOptions {
        DeployOptions {
            app_name: "test-app".into(),
            auto_approve: true,
            dry_run: false,
            working_dir: PathBuf::from("/tmp/test"),
            smoke_timeout: Duration::from_secs(5),
        }
    }

    fn pipeline_with(fly: MockFlyExecutor, router: RouterRef) -> DeployPipeline {
        DeployPipeline {
            config: OpsConfig::default(),
            target: test_target(),
            fly: Arc::new(fly),
            http: Arc::new(MockDeployHttpClient::all_ok(4)),
            router,
            approval: Arc::new(AutoApproveDeployHandler),
        }
    }

    // --- Tests ---

    #[tokio::test]
    async fn happy_path_healthy() {
        let fly = MockFlyExecutor {
            status_output: success_fly(),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let pipeline = pipeline_with(fly, mock_router_ref());
        let result = pipeline.run(&test_opts()).await;

        assert_eq!(result.status, DeployStatus::Healthy);
        assert!(result.stages_completed.contains(&"pre_check".into()));
        assert!(result.stages_completed.contains(&"deploy".into()));
        assert!(result.stages_completed.contains(&"smoke_test".into()));
        assert!(result.stages_completed.contains(&"assessment".into()));
        assert!(result.stages_completed.contains(&"act".into()));
        assert!(result.smoke_report.is_some());
        assert!(result.assessment.is_some());
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn pre_check_failure_other_error() {
        let fly = MockFlyExecutor {
            status_output: failure_fly("authentication required"),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let pipeline = pipeline_with(fly, mock_router_ref());
        let result = pipeline.run(&test_opts()).await;

        assert_eq!(result.status, DeployStatus::PreCheckFailed);
        assert!(result.error.unwrap().contains("pre-check failed"));
    }

    #[tokio::test]
    async fn deploy_failure() {
        let fly = MockFlyExecutor {
            status_output: success_fly(),
            deploy_output: failure_fly("build failed"),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let pipeline = pipeline_with(fly, mock_router_ref());
        let result = pipeline.run(&test_opts()).await;

        assert_eq!(result.status, DeployStatus::DeployFailed);
        assert!(result.error.unwrap().contains("deploy failed"));
    }

    #[tokio::test]
    async fn governance_rejection() {
        let fly = MockFlyExecutor {
            status_output: success_fly(),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let mut config = OpsConfig::default();
        config.governance_settings.require_deploy_approval = true;

        let pipeline = DeployPipeline {
            config,
            target: test_target(),
            fly: Arc::new(fly),
            http: Arc::new(MockDeployHttpClient::all_ok(4)),
            router: mock_router_ref(),
            approval: Arc::new(RejectDeployHandler),
        };

        let mut opts = test_opts();
        opts.auto_approve = false;
        let result = pipeline.run(&opts).await;

        assert_eq!(result.status, DeployStatus::Rejected);
    }

    #[tokio::test]
    async fn dry_run_mode() {
        let fly = MockFlyExecutor {
            status_output: success_fly(),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let pipeline = pipeline_with(fly, mock_router_ref());
        let mut opts = test_opts();
        opts.dry_run = true;
        let result = pipeline.run(&opts).await;

        assert_eq!(result.status, DeployStatus::DryRun);
        assert!(result.smoke_report.is_some());
        assert!(result.assessment.is_none());
        assert!(result.stages_completed.contains(&"pre_check".into()));
        assert!(result.stages_completed.contains(&"dry_run_smoke".into()));
    }

    #[tokio::test]
    async fn rollback_triggered() {
        let unhealthy_response = r#"{
            "assessment": "unhealthy",
            "checks": [],
            "deploy_verified": false,
            "issues": ["required check failed"],
            "recommendation": "rollback",
            "confidence": "high",
            "rollback_target": null
        }"#;
        let router = sequential_router_ref(vec![final_response(unhealthy_response)]);
        let fly = MockFlyExecutor {
            status_output: success_fly(),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let pipeline = pipeline_with(fly, router);
        let result = pipeline.run(&test_opts()).await;

        assert_eq!(result.status, DeployStatus::RolledBack);
    }

    #[tokio::test]
    async fn rollback_failure() {
        let unhealthy_response = r#"{
            "assessment": "unhealthy",
            "checks": [],
            "deploy_verified": false,
            "issues": ["check failed"],
            "recommendation": "rollback",
            "confidence": "high",
            "rollback_target": null
        }"#;
        let router = sequential_router_ref(vec![final_response(unhealthy_response)]);
        let fly = MockFlyExecutor {
            status_output: success_fly(),
            deploy_output: success_fly(),
            rollback_output: failure_fly("rollback failed"),
            create_output: success_fly(),
        };
        let pipeline = pipeline_with(fly, router);
        let result = pipeline.run(&test_opts()).await;

        assert_eq!(result.status, DeployStatus::RollbackFailed);
    }

    #[tokio::test]
    async fn escalation() {
        let escalate_response = r#"{
            "assessment": "unhealthy",
            "checks": [],
            "deploy_verified": false,
            "issues": ["unknown issue"],
            "recommendation": "escalate",
            "confidence": "low",
            "rollback_target": null
        }"#;
        let router = sequential_router_ref(vec![final_response(escalate_response)]);
        let fly = MockFlyExecutor {
            status_output: success_fly(),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let pipeline = pipeline_with(fly, router);
        let result = pipeline.run(&test_opts()).await;

        assert_eq!(result.status, DeployStatus::Escalated);
    }

    #[tokio::test]
    async fn degraded_on_investigate() {
        let degraded_response = r#"{
            "assessment": "degraded",
            "checks": [],
            "deploy_verified": true,
            "issues": ["optional check failed"],
            "recommendation": "investigate",
            "confidence": "medium",
            "rollback_target": null
        }"#;
        let router = sequential_router_ref(vec![final_response(degraded_response)]);
        let fly = MockFlyExecutor {
            status_output: success_fly(),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let pipeline = pipeline_with(fly, router);
        let result = pipeline.run(&test_opts()).await;

        assert_eq!(result.status, DeployStatus::Degraded);
    }

    #[test]
    fn deploy_status_display() {
        assert_eq!(DeployStatus::Healthy.to_string(), "healthy");
        assert_eq!(DeployStatus::Degraded.to_string(), "degraded");
        assert_eq!(DeployStatus::RolledBack.to_string(), "rolled_back");
        assert_eq!(DeployStatus::DeployFailed.to_string(), "deploy_failed");
        assert_eq!(DeployStatus::Escalated.to_string(), "escalated");
        assert_eq!(DeployStatus::PreCheckFailed.to_string(), "pre_check_failed");
        assert_eq!(DeployStatus::Rejected.to_string(), "rejected");
        assert_eq!(DeployStatus::DryRun.to_string(), "dry_run");
        assert_eq!(DeployStatus::RollbackFailed.to_string(), "rollback_failed");
    }

    #[test]
    fn deploy_status_serde_roundtrip() {
        let statuses = vec![
            DeployStatus::Healthy,
            DeployStatus::Degraded,
            DeployStatus::RolledBack,
            DeployStatus::DeployFailed,
            DeployStatus::Escalated,
            DeployStatus::PreCheckFailed,
            DeployStatus::Rejected,
            DeployStatus::DryRun,
            DeployStatus::RollbackFailed,
        ];
        let json = serde_json::to_string(&statuses).unwrap();
        let parsed: Vec<DeployStatus> = serde_json::from_str(&json).unwrap();
        assert_eq!(statuses, parsed);
    }

    #[test]
    fn deploy_options_default() {
        let opts = DeployOptions::default();
        assert_eq!(opts.app_name, "lowendinsight");
        assert!(!opts.auto_approve);
        assert!(!opts.dry_run);
    }

    #[test]
    fn deploy_result_serde_roundtrip() {
        let result = DeployResult {
            status: DeployStatus::Healthy,
            stages_completed: vec!["pre_check".into(), "deploy".into()],
            smoke_report: None,
            assessment: Some(serde_json::json!({"assessment": "healthy"})),
            error: None,
            budget_cost: 0.001,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: DeployResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, DeployStatus::Healthy);
        assert_eq!(parsed.stages_completed.len(), 2);
    }

    #[test]
    fn auto_approve_handler_returns_true() {
        let handler = AutoApproveDeployHandler;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let approved = rt.block_on(handler.request_deploy_approval("app", "summary"));
        assert!(approved);
    }

    #[tokio::test]
    async fn assessment_error_escalates() {
        // Router with no provider registered → complete() returns error
        use glitchlab_kernel::budget::BudgetTracker;
        let routing = std::collections::HashMap::from([(
            "uplink_sre".to_string(),
            "nonexistent/model".to_string(),
        )]);
        let budget = BudgetTracker::new(1_000_000, 100.0);
        let router = Arc::new(glitchlab_router::Router::new(routing, budget));

        let fly = MockFlyExecutor {
            status_output: success_fly(),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let pipeline = pipeline_with(fly, router);
        let result = pipeline.run(&test_opts()).await;

        assert_eq!(result.status, DeployStatus::Escalated);
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("uplink assessment error"));
        assert!(
            result
                .stages_completed
                .contains(&"assessment_failed".into())
        );
    }

    // --- PreCheckOutcome tests ---

    #[test]
    fn classify_precheck_app_exists() {
        let output = success_fly();
        assert_eq!(classify_precheck(&output), PreCheckOutcome::AppExists);
    }

    #[test]
    fn classify_precheck_app_not_found() {
        let output = failure_fly("Error: Could not find app \"myapp\"");
        assert_eq!(classify_precheck(&output), PreCheckOutcome::AppNotFound);
    }

    #[test]
    fn classify_precheck_app_not_found_alternate() {
        let output = failure_fly("app not found");
        assert_eq!(classify_precheck(&output), PreCheckOutcome::AppNotFound);
    }

    #[test]
    fn classify_precheck_other_error() {
        let output = failure_fly("authentication required");
        assert_eq!(
            classify_precheck(&output),
            PreCheckOutcome::OtherError("authentication required".into())
        );
    }

    // --- Auto-create tests ---

    #[tokio::test]
    async fn precheck_app_not_found_auto_creates() {
        let fly = MockFlyExecutor {
            status_output: failure_fly("Could not find app"),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let pipeline = pipeline_with(fly, mock_router_ref());
        let result = pipeline.run(&test_opts()).await;

        assert_eq!(result.status, DeployStatus::Healthy);
        assert!(result.stages_completed.contains(&"app_created".into()));
        assert!(result.stages_completed.contains(&"pre_check".into()));
    }

    #[tokio::test]
    async fn precheck_app_not_found_create_fails() {
        let fly = MockFlyExecutor {
            status_output: failure_fly("Could not find app"),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: failure_fly("org quota exceeded"),
        };
        let pipeline = pipeline_with(fly, mock_router_ref());
        let result = pipeline.run(&test_opts()).await;

        assert_eq!(result.status, DeployStatus::PreCheckFailed);
        assert!(result.error.unwrap().contains("app creation failed"));
    }

    #[tokio::test]
    async fn precheck_app_not_found_create_rejected() {
        let fly = MockFlyExecutor {
            status_output: failure_fly("Could not find app"),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let mut config = OpsConfig::default();
        config.governance_settings.require_deploy_approval = true;

        let pipeline = DeployPipeline {
            config,
            target: test_target(),
            fly: Arc::new(fly),
            http: Arc::new(MockDeployHttpClient::all_ok(4)),
            router: mock_router_ref(),
            approval: Arc::new(RejectDeployHandler),
        };
        let mut opts = test_opts();
        opts.auto_approve = false;
        let result = pipeline.run(&opts).await;

        assert_eq!(result.status, DeployStatus::Rejected);
        assert!(result.error.unwrap().contains("app creation rejected"));
    }

    // --- Dry-run soft-fail tests ---

    #[tokio::test]
    async fn dry_run_precheck_failure_soft_fails() {
        let fly = MockFlyExecutor {
            status_output: failure_fly("Could not find app"),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let pipeline = pipeline_with(fly, mock_router_ref());
        let mut opts = test_opts();
        opts.dry_run = true;
        let result = pipeline.run(&opts).await;

        assert_eq!(result.status, DeployStatus::DryRun);
        assert!(result.stages_completed.contains(&"pre_check_warn".into()));
        assert!(result.stages_completed.contains(&"dry_run_smoke".into()));
    }

    #[tokio::test]
    async fn dry_run_other_error_soft_fails() {
        let fly = MockFlyExecutor {
            status_output: failure_fly("network timeout"),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let pipeline = pipeline_with(fly, mock_router_ref());
        let mut opts = test_opts();
        opts.dry_run = true;
        let result = pipeline.run(&opts).await;

        assert_eq!(result.status, DeployStatus::DryRun);
        assert!(result.stages_completed.contains(&"pre_check_warn".into()));
    }

    // --- to_pipeline_result tests ---

    #[test]
    fn to_pipeline_result_healthy() {
        let result = DeployResult {
            status: DeployStatus::Healthy,
            stages_completed: vec!["pre_check".into()],
            smoke_report: None,
            assessment: None,
            error: None,
            budget_cost: 0.01,
        };
        let pr = result.to_pipeline_result();
        assert_eq!(pr.status, PipelineStatus::Committed);
        assert!((pr.budget.estimated_cost - 0.01).abs() < f64::EPSILON);
    }

    #[test]
    fn to_pipeline_result_dry_run() {
        let result = DeployResult {
            status: DeployStatus::DryRun,
            stages_completed: vec![],
            smoke_report: None,
            assessment: None,
            error: None,
            budget_cost: 0.0,
        };
        let pr = result.to_pipeline_result();
        assert_eq!(pr.status, PipelineStatus::Committed);
    }

    #[test]
    fn to_pipeline_result_deploy_failed() {
        let result = DeployResult {
            status: DeployStatus::DeployFailed,
            stages_completed: vec![],
            smoke_report: None,
            assessment: None,
            error: Some("build error".into()),
            budget_cost: 0.0,
        };
        let pr = result.to_pipeline_result();
        assert_eq!(pr.status, PipelineStatus::ImplementationFailed);
        assert_eq!(pr.error.as_deref(), Some("build error"));
    }

    #[test]
    fn to_pipeline_result_escalated() {
        let result = DeployResult {
            status: DeployStatus::Escalated,
            stages_completed: vec![],
            smoke_report: None,
            assessment: None,
            error: None,
            budget_cost: 0.0,
        };
        let pr = result.to_pipeline_result();
        assert_eq!(pr.status, PipelineStatus::Escalated);
    }

    #[tokio::test]
    async fn deploy_with_fly_toml_path() {
        let fly = MockFlyExecutor {
            status_output: success_fly(),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let mut target = test_target();
        target.fly_toml_path = Some("apps/my_app/fly.toml".into());

        let pipeline = DeployPipeline {
            config: OpsConfig::default(),
            target,
            fly: Arc::new(fly),
            http: Arc::new(MockDeployHttpClient::all_ok(4)),
            router: mock_router_ref(),
            approval: Arc::new(AutoApproveDeployHandler),
        };
        let result = pipeline.run(&test_opts()).await;
        assert_eq!(result.status, DeployStatus::Healthy);
    }

    #[tokio::test]
    async fn auto_create_with_fly_org() {
        let fly = MockFlyExecutor {
            status_output: failure_fly("Could not find app"),
            deploy_output: success_fly(),
            rollback_output: success_fly(),
            create_output: success_fly(),
        };
        let mut target = test_target();
        target.fly_org = Some("my-org".into());

        let pipeline = DeployPipeline {
            config: OpsConfig::default(),
            target,
            fly: Arc::new(fly),
            http: Arc::new(MockDeployHttpClient::all_ok(4)),
            router: mock_router_ref(),
            approval: Arc::new(AutoApproveDeployHandler),
        };
        let result = pipeline.run(&test_opts()).await;
        assert_eq!(result.status, DeployStatus::Healthy);
        assert!(result.stages_completed.contains(&"app_created".into()));
    }

    #[test]
    fn to_pipeline_result_precheck_failed() {
        let result = DeployResult {
            status: DeployStatus::PreCheckFailed,
            stages_completed: vec![],
            smoke_report: None,
            assessment: None,
            error: Some("auth error".into()),
            budget_cost: 0.0,
        };
        let pr = result.to_pipeline_result();
        assert_eq!(pr.status, PipelineStatus::ImplementationFailed);
    }

    #[test]
    fn to_pipeline_result_rolled_back() {
        let result = DeployResult {
            status: DeployStatus::RolledBack,
            stages_completed: vec![],
            smoke_report: None,
            assessment: None,
            error: None,
            budget_cost: 0.0,
        };
        let pr = result.to_pipeline_result();
        assert_eq!(pr.status, PipelineStatus::ImplementationFailed);
    }

    #[test]
    fn to_pipeline_result_rollback_failed() {
        let result = DeployResult {
            status: DeployStatus::RollbackFailed,
            stages_completed: vec![],
            smoke_report: None,
            assessment: None,
            error: None,
            budget_cost: 0.0,
        };
        let pr = result.to_pipeline_result();
        assert_eq!(pr.status, PipelineStatus::ImplementationFailed);
    }

    #[test]
    fn to_pipeline_result_degraded() {
        let result = DeployResult {
            status: DeployStatus::Degraded,
            stages_completed: vec![],
            smoke_report: None,
            assessment: None,
            error: None,
            budget_cost: 0.0,
        };
        let pr = result.to_pipeline_result();
        assert_eq!(pr.status, PipelineStatus::Committed);
    }

    #[test]
    fn to_pipeline_result_rejected() {
        let result = DeployResult {
            status: DeployStatus::Rejected,
            stages_completed: vec![],
            smoke_report: None,
            assessment: None,
            error: None,
            budget_cost: 0.0,
        };
        let pr = result.to_pipeline_result();
        assert_eq!(pr.status, PipelineStatus::Escalated);
    }
}
