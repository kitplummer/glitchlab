//! Functional tests: memory integration end-to-end.
//!
//! Exercises the full loop: pipeline run -> history recorded to composite ->
//! failure context fetched -> agents see it on the next run.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use glitchlab_kernel::agent::Message;
use glitchlab_kernel::budget::{BudgetSummary, BudgetTracker};
use glitchlab_kernel::tool::{ToolCall, ToolDefinition};
use glitchlab_memory::composite::CompositeHistory;
use glitchlab_memory::history::{
    EventsSummary, HistoryBackend, HistoryEntry, HistoryQuery, JsonlHistory,
};
use glitchlab_memory::{MemoryBackendConfig, build_backend};
use glitchlab_router::RouterResponse;
use glitchlab_router::provider::{Provider, ProviderFuture};

use glitchlab_eng_org::config::EngConfig;
use glitchlab_eng_org::pipeline::{AutoApproveHandler, EngineeringPipeline, RealExternalOps};

// ---------------------------------------------------------------------------
// Inline test helpers (duplicated from agents::test_helpers which is pub(crate))
// ---------------------------------------------------------------------------

type RouterRef = Arc<glitchlab_router::Router>;

/// Sequential mock provider: returns pre-scripted responses in order.
struct SequentialMockProvider {
    responses: Mutex<VecDeque<RouterResponse>>,
}

impl SequentialMockProvider {
    fn new(responses: Vec<RouterResponse>) -> Self {
        Self {
            responses: Mutex::new(VecDeque::from(responses)),
        }
    }
}

impl Provider for SequentialMockProvider {
    fn complete(
        &self,
        _model: &str,
        _messages: &[Message],
        _temperature: f32,
        _max_tokens: u32,
        _response_format: Option<&serde_json::Value>,
    ) -> ProviderFuture<'_> {
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| RouterResponse {
                request_id: String::new(), // Set by router
                content: "no more responses".into(),
                model: "mock/test".into(),
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
        _tools: &[ToolDefinition],
        response_format: Option<&serde_json::Value>,
    ) -> ProviderFuture<'_> {
        self.complete(model, messages, temperature, max_tokens, response_format)
    }
}

fn final_response(content: &str) -> RouterResponse {
    RouterResponse {
        request_id: String::new(), // Set by router
        content: content.into(),
        model: "mock/test".into(),
        prompt_tokens: 50,
        completion_tokens: 25,
        total_tokens: 75,
        cost: 0.0005,
        latency_ms: 20,
        tool_calls: vec![],
        stop_reason: Some("end_turn".into()),
    }
}

fn tool_response(calls: Vec<ToolCall>) -> RouterResponse {
    RouterResponse {
        request_id: String::new(), // Set by router
        content: String::new(),
        model: "mock/test".into(),
        prompt_tokens: 50,
        completion_tokens: 25,
        total_tokens: 75,
        cost: 0.0005,
        latency_ms: 20,
        tool_calls: calls,
        stop_reason: Some("tool_use".into()),
    }
}

/// Full pipeline mock responses: planner → implementer (tool + final) →
/// security → release → archivist.
fn pipeline_mock_responses() -> Vec<RouterResponse> {
    vec![
        // 1. Planner
        final_response(
            r#"{"steps": [{"step_number": 1, "description": "add feature", "files": ["src/new.rs"], "action": "create"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "trivial", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
        ),
        // 2. Implementer — write_file tool call
        tool_response(vec![ToolCall {
            id: "toolu_01".into(),
            name: "write_file".into(),
            input: serde_json::json!({"path": "src/new.rs", "content": "pub fn greet() -> &'static str { \"hello\" }\n"}),
        }]),
        // 3. Implementer — final metadata
        final_response(
            r#"{"files_changed": ["src/new.rs"], "tests_added": [], "commit_message": "feat: add greet function", "summary": "test"}"#,
        ),
        // 4. Security
        final_response(r#"{"verdict": "pass", "issues": [], "summary": "no issues"}"#),
        // 5. Release
        final_response(
            r#"{"version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": []}"#,
        ),
        // 6. Archivist
        final_response(
            r#"{"adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#,
        ),
    ]
}

/// Pipeline mock responses for failing-test scenario:
/// planner → implementer (tool + final) → debugger (should_retry: false).
fn pipeline_debug_fail_responses() -> Vec<RouterResponse> {
    vec![
        // 1. Planner
        final_response(
            r#"{"steps": [{"step_number": 1, "description": "fix bug"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false}"#,
        ),
        // 2. Implementer — write_file tool call
        tool_response(vec![ToolCall {
            id: "toolu_01".into(),
            name: "write_file".into(),
            input: serde_json::json!({"path": "src/new.rs", "content": "pub fn greet() -> &'static str { \"hello\" }\n"}),
        }]),
        // 3. Implementer — final metadata
        final_response(
            r#"{"files_changed": ["src/new.rs"], "tests_added": [], "commit_message": "fix: bug", "summary": "fixed"}"#,
        ),
        // 4. Debugger — should_retry: false
        final_response(
            r#"{"diagnosis": "test failure", "root_cause": "bug", "files_changed": [], "confidence": "low", "should_retry": false, "notes": null}"#,
        ),
    ]
}

fn sequential_router_ref(responses: Vec<RouterResponse>) -> RouterRef {
    let routing = HashMap::from([
        ("planner".to_string(), "seq/test".to_string()),
        ("implementer".to_string(), "seq/test".to_string()),
        ("debugger".to_string(), "seq/test".to_string()),
        ("security".to_string(), "seq/test".to_string()),
        ("release".to_string(), "seq/test".to_string()),
        ("archivist".to_string(), "seq/test".to_string()),
    ]);
    let budget = BudgetTracker::new(1_000_000, 100.0);
    let mut router = glitchlab_router::Router::new(routing, budget);
    router.register_provider(
        "seq".into(),
        Arc::new(SequentialMockProvider::new(responses)),
    );
    Arc::new(router)
}

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

fn sample_entry(task_id: &str, status: &str) -> HistoryEntry {
    HistoryEntry {
        timestamp: chrono::Utc::now(),
        task_id: task_id.into(),
        status: status.into(),
        pr_url: None,
        branch: Some(format!("glitchlab/{task_id}")),
        error: if status != "pr_created" && status != "committed" {
            Some("something broke".into())
        } else {
            None
        },
        budget: BudgetSummary::default(),
        events_summary: EventsSummary::default(),
        stage_outputs: None,
        events: None,
        outcome_context: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full E2E: pipeline run -> history fanout -> entries readable from both backends.
#[tokio::test]
async fn pipeline_records_history_to_composite() {
    let repo_dir = tempfile::tempdir().unwrap();
    let base_branch = init_test_repo(repo_dir.path());

    // Two JSONL backends in separate tempdirs, wrapped in CompositeHistory.
    let hist_dir1 = tempfile::tempdir().unwrap();
    let hist_dir2 = tempfile::tempdir().unwrap();
    let b1: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(hist_dir1.path()));
    let b2: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(hist_dir2.path()));
    let composite: Arc<dyn HistoryBackend> =
        Arc::new(CompositeHistory::new(vec![b1.clone(), b2.clone()]));

    // Build pipeline with sequential mock router + auto-approve + composite history.
    let router = sequential_router_ref(pipeline_mock_responses());
    let mut config = EngConfig::default();
    config.intervention.pause_after_plan = false;
    config.intervention.pause_before_pr = false;

    let handler = Arc::new(AutoApproveHandler);
    let pipeline = EngineeringPipeline::new(
        router,
        config,
        handler,
        composite.clone(),
        Arc::new(RealExternalOps),
    );

    // Run pipeline for "task-1".
    let _result = pipeline
        .run(
            "task-1",
            "Add a feature",
            repo_dir.path(),
            &base_branch,
            &[],
        )
        .await;

    // Assert: both JSONL backends have an entry for "task-1" (proves fanout).
    let query = HistoryQuery {
        limit: 10,
        ..Default::default()
    };
    let entries1 = b1.query(&query).await.unwrap();
    let entries2 = b2.query(&query).await.unwrap();
    assert!(
        !entries1.is_empty(),
        "backend 1 should have at least one entry"
    );
    assert!(
        !entries2.is_empty(),
        "backend 2 should have at least one entry"
    );
    assert_eq!(entries1[0].task_id, "task-1");
    assert_eq!(entries2[0].task_id, "task-1");

    // Assert: composite stats returns total_runs >= 1.
    let stats = composite.stats().await.unwrap();
    assert!(
        stats.total_runs >= 1,
        "expected total_runs >= 1, got {}",
        stats.total_runs
    );
}

/// Proves: failure recorded -> failure_context() returns it -> second run sees it.
#[tokio::test]
async fn failure_context_flows_across_runs() {
    let repo_dir = tempfile::tempdir().unwrap();
    let base_branch = init_test_repo(repo_dir.path());

    // Makefile with test target that always fails.
    std::fs::write(
        repo_dir.path().join("Makefile"),
        "test:\n\t@echo FAIL && exit 1\n",
    )
    .unwrap();
    std::process::Command::new("git")
        .args(["add", "Makefile"])
        .current_dir(repo_dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "add failing test"])
        .current_dir(repo_dir.path())
        .output()
        .unwrap();

    let hist_dir = tempfile::tempdir().unwrap();
    let jsonl: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(hist_dir.path()));
    let composite: Arc<dyn HistoryBackend> = Arc::new(CompositeHistory::new(vec![jsonl.clone()]));

    // Run 1: pipeline for "task-fail" -> should fail (TestsFailed).
    let router = sequential_router_ref(pipeline_debug_fail_responses());
    let mut config = EngConfig::default();
    config.intervention.pause_after_plan = false;
    config.intervention.pause_before_pr = false;

    let handler = Arc::new(AutoApproveHandler);
    let pipeline = EngineeringPipeline::new(
        router,
        config,
        handler,
        composite.clone(),
        Arc::new(RealExternalOps),
    );

    let _result = pipeline
        .run(
            "task-fail",
            "Fix the broken tests",
            repo_dir.path(),
            &base_branch,
            &[],
        )
        .await;

    // Assert: history has entry with non-success status.
    let query = HistoryQuery {
        limit: 10,
        ..Default::default()
    };
    let entries = composite.query(&query).await.unwrap();
    assert!(
        !entries.is_empty(),
        "should have at least one history entry"
    );
    assert_ne!(
        entries[0].status, "pr_created",
        "status should not be pr_created for a failing run"
    );
    assert_ne!(
        entries[0].status, "committed",
        "status should not be committed for a failing run"
    );

    // Assert: failure_context returns string containing "task-fail".
    let ctx = composite.failure_context(5).await.unwrap();
    assert!(
        ctx.contains("task-fail"),
        "failure_context should mention task-fail, got: {ctx}"
    );

    // Swap to succeeding Makefile test target.
    std::fs::write(
        repo_dir.path().join("Makefile"),
        "test:\n\t@echo tests pass\n",
    )
    .unwrap();
    std::process::Command::new("git")
        .args(["add", "Makefile"])
        .current_dir(repo_dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "fix tests"])
        .current_dir(repo_dir.path())
        .output()
        .unwrap();

    // Run 2: pipeline for "task-2".
    let router2 = sequential_router_ref(pipeline_mock_responses());
    let mut config2 = EngConfig::default();
    config2.intervention.pause_after_plan = false;
    config2.intervention.pause_before_pr = false;

    let handler2 = Arc::new(AutoApproveHandler);
    let pipeline2 = EngineeringPipeline::new(
        router2,
        config2,
        handler2,
        composite.clone(),
        Arc::new(RealExternalOps),
    );

    let _result2 = pipeline2
        .run(
            "task-2",
            "Add a feature",
            repo_dir.path(),
            &base_branch,
            &[],
        )
        .await;

    // Assert: history now has >= 2 entries.
    let all_entries = composite.query(&query).await.unwrap();
    assert!(
        all_entries.len() >= 2,
        "expected >= 2 entries, got {}",
        all_entries.len()
    );
}

/// Proves the factory function works end-to-end.
#[tokio::test]
async fn build_backend_factory_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let config = MemoryBackendConfig::default();
    let backend = build_backend(dir.path(), &config).await;

    // Record an entry.
    let entry = sample_entry("factory-task", "pr_created");
    backend.record(&entry).await.unwrap();

    // Query it back.
    let query = HistoryQuery {
        limit: 10,
        ..Default::default()
    };
    let entries = backend.query(&query).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].task_id, "factory-task");
    assert_eq!(entries[0].status, "pr_created");

    // Assert stats.
    let stats = backend.stats().await.unwrap();
    assert_eq!(stats.total_runs, 1);
}

/// Proves history survives pipeline reconstruction (not in-memory).
#[tokio::test]
async fn history_persists_across_pipeline_instances() {
    let repo_dir = tempfile::tempdir().unwrap();
    let base_branch = init_test_repo(repo_dir.path());
    let hist_dir = tempfile::tempdir().unwrap();

    // Pipeline instance #1: run and record history.
    {
        let jsonl: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(hist_dir.path()));
        let composite: Arc<dyn HistoryBackend> = Arc::new(CompositeHistory::new(vec![jsonl]));

        let router = sequential_router_ref(pipeline_mock_responses());
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let pipeline = EngineeringPipeline::new(
            router,
            config,
            handler,
            composite,
            Arc::new(RealExternalOps),
        );

        let _result = pipeline
            .run(
                "persist-task",
                "Add a feature",
                repo_dir.path(),
                &base_branch,
                &[],
            )
            .await;
    }

    // Pipeline instance #2: new JsonlHistory from same dir, new CompositeHistory,
    // new pipeline. Can it see entries from run #1?
    {
        let jsonl2: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(hist_dir.path()));
        let composite2: Arc<dyn HistoryBackend> = Arc::new(CompositeHistory::new(vec![jsonl2]));

        let query = HistoryQuery {
            limit: 10,
            ..Default::default()
        };
        let entries = composite2.query(&query).await.unwrap();
        assert!(
            !entries.is_empty(),
            "new history instance should see entries from run #1"
        );
        assert_eq!(entries[0].task_id, "persist-task");

        let stats = composite2.stats().await.unwrap();
        assert!(
            stats.total_runs >= 1,
            "stats should reflect run #1, got {}",
            stats.total_runs
        );
    }
}
