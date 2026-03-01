//! Composite backend that fans writes to all backends and reads from the first available.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tracing::warn;

use crate::error::{MemoryError, Result};
use crate::history::{HistoryBackend, HistoryEntry, HistoryQuery, HistoryStats};

/// A composite backend that writes to all backends and reads from the first available.
///
/// **Writes**: fan-out to all available backends; log `warn!` on secondary failures;
/// only error if ALL fail.
///
/// **Reads**: first available backend in priority order (the order they were added).
///
/// **`is_available`**: true if any backend is available.
pub struct CompositeHistory {
    backends: Vec<Arc<dyn HistoryBackend>>,
}

impl CompositeHistory {
    /// Create a composite from a list of backends (priority order: first = highest).
    pub fn new(backends: Vec<Arc<dyn HistoryBackend>>) -> Self {
        Self { backends }
    }
}

impl HistoryBackend for CompositeHistory {
    fn record<'a>(
        &'a self,
        entry: &'a HistoryEntry,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if self.backends.is_empty() {
                return Ok(());
            }

            let mut first_ok = false;
            let mut last_err = None;

            for (i, backend) in self.backends.iter().enumerate() {
                match backend.record(entry).await {
                    Ok(()) => first_ok = true,
                    Err(e) => {
                        if i == 0 {
                            warn!(
                                backend = backend.backend_name(),
                                error = %e,
                                "primary backend record failed"
                            );
                        } else {
                            warn!(
                                backend = backend.backend_name(),
                                error = %e,
                                "secondary backend record failed"
                            );
                        }
                        last_err = Some(e);
                    }
                }
            }

            if first_ok {
                Ok(())
            } else {
                Err(last_err
                    .unwrap_or_else(|| MemoryError::Unavailable("no backends available".into())))
            }
        })
    }

    fn query<'a>(
        &'a self,
        query: &'a HistoryQuery,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<HistoryEntry>>> + Send + 'a>> {
        Box::pin(async move {
            for backend in &self.backends {
                if backend.is_available().await {
                    match backend.query(query).await {
                        Ok(entries) => return Ok(entries),
                        Err(e) => {
                            warn!(
                                backend = backend.backend_name(),
                                error = %e,
                                "query failed, trying next backend"
                            );
                        }
                    }
                }
            }
            Ok(Vec::new())
        })
    }

    fn stats(&self) -> Pin<Box<dyn Future<Output = Result<HistoryStats>> + Send + '_>> {
        Box::pin(async {
            for backend in &self.backends {
                if backend.is_available().await {
                    match backend.stats().await {
                        Ok(stats) => return Ok(stats),
                        Err(e) => {
                            warn!(
                                backend = backend.backend_name(),
                                error = %e,
                                "stats failed, trying next backend"
                            );
                        }
                    }
                }
            }
            Ok(HistoryStats {
                total_runs: 0,
                successes: 0,
                failures: 0,
                total_cost: 0.0,
                total_tokens: 0,
            })
        })
    }

    fn failure_context(
        &self,
        max_entries: usize,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        Box::pin(async move {
            for backend in &self.backends {
                if backend.is_available().await {
                    match backend.failure_context(max_entries).await {
                        Ok(ctx) => return Ok(ctx),
                        Err(e) => {
                            warn!(
                                backend = backend.backend_name(),
                                error = %e,
                                "failure_context failed, trying next backend"
                            );
                        }
                    }
                }
            }
            Ok(String::new())
        })
    }

    fn backend_name(&self) -> &str {
        "composite"
    }

    fn is_available(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async {
            for backend in &self.backends {
                if backend.is_available().await {
                    return true;
                }
            }
            false
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::JsonlHistory;
    use chrono::{TimeZone, Utc};
    use glitchlab_kernel::budget::BudgetSummary;

    fn sample_entry(task_id: &str, status: &str) -> HistoryEntry {
        HistoryEntry {
            timestamp: Utc.with_ymd_and_hms(2026, 2, 21, 14, 30, 0).unwrap(),
            task_id: task_id.into(),
            status: status.into(),
            pr_url: None,
            branch: None,
            error: None,
            budget: BudgetSummary::default(),
            events_summary: crate::history::EventsSummary::default(),
            stage_outputs: None,
            events: None,
            outcome_context: None,
        }
    }

    #[tokio::test]
    async fn single_jsonl_record_and_query() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let composite = CompositeHistory::new(vec![jsonl]);

        composite
            .record(&sample_entry("t1", "pr_created"))
            .await
            .unwrap();
        let query = HistoryQuery {
            limit: 10,
            ..Default::default()
        };
        let entries = composite.query(&query).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].task_id, "t1");
    }

    #[tokio::test]
    async fn write_goes_to_both_backends() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let b1: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir1.path()));
        let b2: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir2.path()));
        let composite = CompositeHistory::new(vec![b1.clone(), b2.clone()]);

        composite
            .record(&sample_entry("t1", "pr_created"))
            .await
            .unwrap();

        let query = HistoryQuery {
            limit: 10,
            ..Default::default()
        };
        // Both backends should have the entry.
        let entries1 = b1.query(&query).await.unwrap();
        let entries2 = b2.query(&query).await.unwrap();
        assert_eq!(entries1.len(), 1);
        assert_eq!(entries2.len(), 1);
    }

    #[tokio::test]
    async fn reads_from_first_available() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let b1: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir1.path()));
        let b2: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir2.path()));

        // Write directly to b2, not b1
        b2.record(&sample_entry("t-from-b2", "error"))
            .await
            .unwrap();

        // Composite reads from b1 first (which is empty).
        let composite = CompositeHistory::new(vec![b1, b2]);
        let query = HistoryQuery {
            limit: 10,
            ..Default::default()
        };
        let entries = composite.query(&query).await.unwrap();
        // b1 is available and returns empty.
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn empty_backends_returns_empty() {
        let composite = CompositeHistory::new(vec![]);
        let query = HistoryQuery::default();
        let entries = composite.query(&query).await.unwrap();
        assert!(entries.is_empty());

        // Record on empty composite should succeed (no-op).
        composite.record(&sample_entry("t1", "ok")).await.unwrap();

        // Stats on empty composite should return zeros.
        let stats = composite.stats().await.unwrap();
        assert_eq!(stats.total_runs, 0);
    }

    #[tokio::test]
    async fn is_available_with_backends() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let composite = CompositeHistory::new(vec![jsonl]);
        assert!(composite.is_available().await);
    }

    #[tokio::test]
    async fn is_available_empty() {
        let composite = CompositeHistory::new(vec![]);
        assert!(!composite.is_available().await);
    }

    #[test]
    fn backend_name_is_composite() {
        let composite = CompositeHistory::new(vec![]);
        assert_eq!(composite.backend_name(), "composite");
    }

    #[tokio::test]
    async fn secondary_write_failure_still_ok() {
        // Use a beads client pointing to a nonexistent binary as the "failing" backend.
        let dir = tempfile::tempdir().unwrap();
        let good: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let bad: Arc<dyn HistoryBackend> = Arc::new(crate::beads::BeadsClient::new(
            dir.path(),
            Some("nonexistent-bd-xyz".into()),
        ));
        let composite = CompositeHistory::new(vec![good, bad]);

        // Should succeed because the primary (JSONL) succeeds.
        composite
            .record(&sample_entry("t1", "pr_created"))
            .await
            .unwrap();

        let query = HistoryQuery {
            limit: 10,
            ..Default::default()
        };
        let entries = composite.query(&query).await.unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn stats_through_composite() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let composite = CompositeHistory::new(vec![jsonl]);

        composite
            .record(&sample_entry("t1", "pr_created"))
            .await
            .unwrap();
        composite
            .record(&sample_entry("t2", "error"))
            .await
            .unwrap();

        let stats = composite.stats().await.unwrap();
        assert_eq!(stats.total_runs, 2);
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.failures, 1);
    }

    #[tokio::test]
    async fn failure_context_through_composite() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let composite = CompositeHistory::new(vec![jsonl]);

        composite
            .record(&sample_entry("t-fail", "error"))
            .await
            .unwrap();

        let ctx = composite.failure_context(5).await.unwrap();
        assert!(ctx.contains("t-fail"));
    }

    #[tokio::test]
    async fn failure_context_empty_when_no_failures() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let composite = CompositeHistory::new(vec![jsonl]);

        composite
            .record(&sample_entry("t1", "pr_created"))
            .await
            .unwrap();

        let ctx = composite.failure_context(5).await.unwrap();
        assert!(ctx.is_empty());
    }

    /// A backend that reports as available but fails on all operations.
    /// Used to test composite fallback paths.
    struct FailingBackend;

    impl HistoryBackend for FailingBackend {
        fn record<'a>(
            &'a self,
            _entry: &'a HistoryEntry,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async { Err(MemoryError::Unavailable("failing backend".into())) })
        }

        fn query<'a>(
            &'a self,
            _query: &'a HistoryQuery,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<HistoryEntry>>> + Send + 'a>>
        {
            Box::pin(async { Err(MemoryError::Unavailable("failing backend".into())) })
        }

        fn stats(
            &self,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<HistoryStats>> + Send + '_>> {
            Box::pin(async { Err(MemoryError::Unavailable("failing backend".into())) })
        }

        fn failure_context(
            &self,
            _max_entries: usize,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>> {
            Box::pin(async { Err(MemoryError::Unavailable("failing backend".into())) })
        }

        fn backend_name(&self) -> &str {
            "failing"
        }

        fn is_available(&self) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            Box::pin(async { true })
        }
    }

    #[tokio::test]
    async fn query_falls_through_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let failing: Arc<dyn HistoryBackend> = Arc::new(FailingBackend);
        let good: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));

        // Write an entry to the good backend.
        good.record(&sample_entry("t1", "ok")).await.unwrap();

        let composite = CompositeHistory::new(vec![failing, good]);
        let query = HistoryQuery {
            limit: 10,
            ..Default::default()
        };
        // First backend fails, falls through to second.
        let entries = composite.query(&query).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].task_id, "t1");
    }

    #[tokio::test]
    async fn stats_falls_through_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let failing: Arc<dyn HistoryBackend> = Arc::new(FailingBackend);
        let good: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));

        good.record(&sample_entry("t1", "pr_created"))
            .await
            .unwrap();
        good.record(&sample_entry("t2", "error")).await.unwrap();

        let composite = CompositeHistory::new(vec![failing, good]);
        let stats = composite.stats().await.unwrap();
        assert_eq!(stats.total_runs, 2);
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.failures, 1);
    }

    #[tokio::test]
    async fn failure_context_falls_through_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let failing: Arc<dyn HistoryBackend> = Arc::new(FailingBackend);
        let good: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));

        good.record(&sample_entry("t-fail", "error")).await.unwrap();

        let composite = CompositeHistory::new(vec![failing, good]);
        let ctx = composite.failure_context(5).await.unwrap();
        assert!(ctx.contains("t-fail"));
    }

    #[tokio::test]
    async fn all_backends_fail_record_returns_error() {
        let bad1: Arc<dyn HistoryBackend> = Arc::new(FailingBackend);
        let bad2: Arc<dyn HistoryBackend> = Arc::new(FailingBackend);
        let composite = CompositeHistory::new(vec![bad1, bad2]);

        let result = composite.record(&sample_entry("t1", "ok")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn primary_record_failure_secondary_ok() {
        let dir = tempfile::tempdir().unwrap();
        let bad: Arc<dyn HistoryBackend> = Arc::new(FailingBackend);
        let good: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        // FailingBackend is primary (i == 0), JSONL is secondary
        let composite = CompositeHistory::new(vec![bad, good]);

        // Should succeed because the secondary (JSONL) succeeds.
        // Covers lines 50-52: "primary backend record failed" warn log.
        composite
            .record(&sample_entry("t1", "pr_created"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn secondary_record_failure_primary_ok() {
        let dir = tempfile::tempdir().unwrap();
        let good: Arc<dyn HistoryBackend> = Arc::new(JsonlHistory::new(dir.path()));
        let bad: Arc<dyn HistoryBackend> = Arc::new(FailingBackend);
        // JSONL is primary (i == 0), FailingBackend is secondary (i == 1)
        let composite = CompositeHistory::new(vec![good, bad]);

        // Should succeed because the primary (JSONL) succeeds.
        // Covers lines 56-58: "secondary backend record failed" warn log.
        composite
            .record(&sample_entry("t1", "pr_created"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failure_context_all_fail_returns_empty() {
        let bad1: Arc<dyn HistoryBackend> = Arc::new(FailingBackend);
        let bad2: Arc<dyn HistoryBackend> = Arc::new(FailingBackend);
        let composite = CompositeHistory::new(vec![bad1, bad2]);

        // All backends fail failure_context, should return Ok("").
        // Covers line 143: empty-string fallback when all backends fail.
        let ctx = composite.failure_context(5).await.unwrap();
        assert!(ctx.is_empty());
    }

    #[tokio::test]
    async fn stats_all_fail_returns_defaults() {
        let bad1: Arc<dyn HistoryBackend> = Arc::new(FailingBackend);
        let bad2: Arc<dyn HistoryBackend> = Arc::new(FailingBackend);
        let composite = CompositeHistory::new(vec![bad1, bad2]);

        // All backends fail stats, should return default zero stats.
        let stats = composite.stats().await.unwrap();
        assert_eq!(stats.total_runs, 0);
        assert_eq!(stats.successes, 0);
        assert_eq!(stats.failures, 0);
    }

    #[tokio::test]
    async fn query_all_fail_returns_empty() {
        let bad1: Arc<dyn HistoryBackend> = Arc::new(FailingBackend);
        let bad2: Arc<dyn HistoryBackend> = Arc::new(FailingBackend);
        let composite = CompositeHistory::new(vec![bad1, bad2]);

        let query = HistoryQuery {
            limit: 10,
            ..Default::default()
        };
        // All backends fail query, should return empty vec.
        let entries = composite.query(&query).await.unwrap();
        assert!(entries.is_empty());
    }
}
