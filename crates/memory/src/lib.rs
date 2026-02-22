pub mod beads;
pub mod composite;
#[cfg(feature = "dolt")]
pub mod dolt;
pub mod error;
pub mod history;

use std::path::Path;
use std::sync::Arc;

use history::HistoryBackend;

/// Configuration for building a memory backend.
///
/// Defined in the memory crate so it doesn't depend on eng-org.
/// `EngConfig` maps its `MemoryConfig` to this when calling the factory.
#[derive(Debug, Clone, Default)]
pub struct MemoryBackendConfig {
    /// MySQL/Dolt connection string. If `None`, Dolt is not used.
    pub dolt_connection: Option<String>,
    /// Whether to enable the Beads graph backend.
    pub beads_enabled: bool,
    /// Path to the `bd` binary. If `None`, uses `BEADS_BD_PATH` or `"bd"` on PATH.
    pub beads_bd_path: Option<String>,
}

/// Build a composite backend from config.
///
/// Tries Dolt (if configured) -> Beads (if enabled and `bd` installed) -> always adds JSONL.
/// Returns `Arc<dyn HistoryBackend>`.
pub async fn build_backend(
    repo_path: &Path,
    config: &MemoryBackendConfig,
) -> Arc<dyn HistoryBackend> {
    let mut backends: Vec<Arc<dyn HistoryBackend>> = Vec::new();

    // --- Dolt (if configured) ---
    #[cfg(feature = "dolt")]
    if let Some(ref conn) = config.dolt_connection {
        match dolt::DoltHistory::new(conn).await {
            Ok(dh) => {
                if let Err(e) = dh.ensure_schema().await {
                    tracing::warn!(error = %e, "Dolt schema creation failed, skipping Dolt");
                } else {
                    tracing::info!("Dolt backend connected");
                    backends.push(Arc::new(dh));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Dolt connection failed, skipping Dolt");
            }
        }
    }

    // --- Beads (if enabled and installed) ---
    if config.beads_enabled {
        let beads = beads::BeadsClient::new(repo_path, config.beads_bd_path.clone());
        if beads.check_installed().await {
            tracing::info!("Beads backend available");
            backends.push(Arc::new(beads));
        } else {
            tracing::warn!("Beads enabled but bd binary not found, skipping");
        }
    }

    // --- JSONL (always) ---
    backends.push(Arc::new(history::JsonlHistory::new(repo_path)));

    Arc::new(composite::CompositeHistory::new(backends))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_backend_config_default() {
        let config = MemoryBackendConfig::default();
        assert!(config.dolt_connection.is_none());
        assert!(!config.beads_enabled);
        assert!(config.beads_bd_path.is_none());
    }

    #[tokio::test]
    async fn build_backend_default_config_returns_working_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryBackendConfig::default();
        let backend = build_backend(dir.path(), &config).await;

        assert_eq!(backend.backend_name(), "composite");
        assert!(backend.is_available().await);

        // Record and query should work.
        let entry = history::HistoryEntry {
            timestamp: chrono::Utc::now(),
            task_id: "factory-test".into(),
            status: "pr_created".into(),
            pr_url: None,
            branch: None,
            error: None,
            budget: glitchlab_kernel::budget::BudgetSummary::default(),
            events_summary: history::EventsSummary::default(),
            stage_outputs: None,
            events: None,
        };
        backend.record(&entry).await.unwrap();

        let query = history::HistoryQuery {
            limit: 10,
            ..Default::default()
        };
        let entries = backend.query(&query).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].task_id, "factory-test");
    }

    #[tokio::test]
    async fn build_backend_with_beads_enabled_but_missing_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryBackendConfig {
            beads_enabled: true,
            beads_bd_path: Some("nonexistent-bd-xyz-factory".into()),
            ..Default::default()
        };
        let backend = build_backend(dir.path(), &config).await;
        assert!(backend.is_available().await);
    }
}
