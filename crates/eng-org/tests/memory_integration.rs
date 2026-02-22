//! Functional tests: memory integration end-to-end.
//!
//! Exercises the full loop: pipeline run -> history recorded to composite ->
//! failure context fetched -> agents see it on the next run.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use glitchlab_kernel::agent::Message;
use glitchlab_kernel::budget::{BudgetSummary, BudgetTracker};
use glitchlab_memory::composite::CompositeHistory;
use glitchlab_memory::history::{
    EventsSummary, HistoryBackend, HistoryEntry, HistoryQuery, JsonlHistory,
};
use glitchlab_memory::{MemoryBackendConfig, build_backend};
use glitchlab_router::RouterResponse;
use glitchlab_router::provider::{Provider, ProviderFuture};

use glitchlab_eng_org::config::EngConfig;
use glitchlab_eng_org::pipeline::{AutoApproveHandler, EngineeringPipeline};

// ---------------------------------------------------------------------------
// Inline test helpers (duplicated from agents::test_helpers which is pub(crate))
// ---------------------------------------------------------------------------

type RouterRef = Arc<glitchlab_router::Router>;

struct MockProvider;

impl Provider for MockProvider {
    fn complete(
        &self,
        _model: &str,
        _messages: &[Message],
        _temperature: f32,
        _max_tokens: u32,
        _response_format: Option<&serde_json::Value>,
    ) -> ProviderFuture<'_> {
        Box::pin(async move {
            Ok(RouterResponse {
                content: r#"{"result": "ok", "steps": [{"step_number": 1, "description": "add feature", "files": ["src/new.rs"], "action": "create"}], "files_likely_affected": ["src/new.rs"], "requires_core_change": false, "risk_level": "low", "risk_notes": "trivial", "test_strategy": [], "estimated_complexity": "trivial", "dependencies_affected": false, "public_api_changed": false, "verdict": "pass", "issues": [], "version_bump": "patch", "reasoning": "test", "changelog_entry": "", "breaking_changes": [], "diagnosis": "none", "root_cause": "none", "fix": {"changes": []}, "confidence": "high", "should_retry": false, "changes": [{"file": "src/new.rs", "action": "create", "content": "pub fn greet() -> &'static str { \"hello\" }\n", "description": "add greet function"}], "tests_added": [], "commit_message": "feat: add greet function", "summary": "test", "adr": null, "doc_updates": [], "architecture_notes": "", "should_write_adr": false}"#.into(),
                model: "mock/test-model".into(),
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cost: 0.001,
                latency_ms: 42,
                tool_calls: vec![],
                stop_reason: None,
            })
        })
    }
}

fn mock_router_ref() -> RouterRef {
    let routing = HashMap::from([
        ("planner".to_string(), "mock/test".to_string()),
        ("implementer".to_string(), "mock/test".to_string()),
        ("debugger".to_string(), "mock/test".to_string()),
        ("security".to_string(), "mock/test".to_string()),
        ("release".to_string(), "mock/test".to_string()),
        ("archivist".to_string(), "mock/test".to_string()),
    ]);
    let budget = BudgetTracker::new(1_000_000, 100.0);
    let mut router = glitchlab_router::Router::new(routing, budget);
    router.register_provider("mock".into(), Arc::new(MockProvider));
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

    // Build pipeline with mock router + auto-approve + composite history.
    let router = mock_router_ref();
    let mut config = EngConfig::default();
    config.intervention.pause_after_plan = false;
    config.intervention.pause_before_pr = false;

    let handler = Arc::new(AutoApproveHandler);
    let pipeline = EngineeringPipeline::new(router, config, handler, composite.clone());

    // Run pipeline for "task-1".
    let _result = pipeline
        .run("task-1", "Add a feature", repo_dir.path(), &base_branch)
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
    let router = mock_router_ref();
    let mut config = EngConfig::default();
    config.intervention.pause_after_plan = false;
    config.intervention.pause_before_pr = false;

    let handler = Arc::new(AutoApproveHandler);
    let pipeline = EngineeringPipeline::new(router, config, handler, composite.clone());

    let _result = pipeline
        .run(
            "task-fail",
            "Fix the broken tests",
            repo_dir.path(),
            &base_branch,
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
    let router2 = mock_router_ref();
    let mut config2 = EngConfig::default();
    config2.intervention.pause_after_plan = false;
    config2.intervention.pause_before_pr = false;

    let handler2 = Arc::new(AutoApproveHandler);
    let pipeline2 = EngineeringPipeline::new(router2, config2, handler2, composite.clone());

    let _result2 = pipeline2
        .run("task-2", "Add a feature", repo_dir.path(), &base_branch)
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

        let router = mock_router_ref();
        let mut config = EngConfig::default();
        config.intervention.pause_after_plan = false;
        config.intervention.pause_before_pr = false;

        let handler = Arc::new(AutoApproveHandler);
        let pipeline = EngineeringPipeline::new(router, config, handler, composite);

        let _result = pipeline
            .run(
                "persist-task",
                "Add a feature",
                repo_dir.path(),
                &base_branch,
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
