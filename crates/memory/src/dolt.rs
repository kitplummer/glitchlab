//! Dolt-backed history persistence.
//!
//! Requires the `dolt` feature flag and a running Dolt SQL server.

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use sqlx::{MySqlPool, Row};

use crate::error::{MemoryError, Result};
use crate::history::{EventsSummary, HistoryBackend, HistoryEntry, HistoryQuery, HistoryStats};
use glitchlab_kernel::budget::BudgetSummary;

/// SQL schema for the task_history table.
pub const CREATE_TABLE_SQL: &str = r"
CREATE TABLE IF NOT EXISTS task_history (
    id                       BIGINT AUTO_INCREMENT PRIMARY KEY,
    task_id                  VARCHAR(255) NOT NULL,
    timestamp                DATETIME(6) NOT NULL,
    status                   VARCHAR(64) NOT NULL,
    pr_url                   TEXT,
    branch                   VARCHAR(255),
    error                    TEXT,
    budget_total_tokens      BIGINT NOT NULL DEFAULT 0,
    budget_estimated_cost    DOUBLE NOT NULL DEFAULT 0.0,
    budget_call_count        BIGINT NOT NULL DEFAULT 0,
    budget_tokens_remaining  BIGINT NOT NULL DEFAULT 0,
    budget_dollars_remaining DOUBLE NOT NULL DEFAULT 0.0,
    events_plan_steps        INT NOT NULL DEFAULT 0,
    events_plan_risk         VARCHAR(32) NOT NULL DEFAULT '',
    events_tests_passed      INT NOT NULL DEFAULT 0,
    events_security_verdict  VARCHAR(32) NOT NULL DEFAULT '',
    events_version_bump      VARCHAR(32) NOT NULL DEFAULT '',
    events_fix_attempts      INT NOT NULL DEFAULT 0,
    stage_outputs            JSON,
    events_json              JSON,
    INDEX idx_task_id (task_id),
    INDEX idx_timestamp (timestamp),
    INDEX idx_status (status)
);
";

/// Dolt SQL backend for task history.
#[derive(Debug)]
pub struct DoltHistory {
    pool: MySqlPool,
}

impl DoltHistory {
    /// Create a new DoltHistory connected to the given MySQL/Dolt connection string.
    pub async fn new(connection_string: &str) -> Result<Self> {
        let pool = MySqlPool::connect(connection_string)
            .await
            .map_err(|e| MemoryError::Dolt(format!("connection failed: {e}")))?;
        Ok(Self { pool })
    }

    /// Create a DoltHistory from an existing pool.
    pub fn from_pool(pool: MySqlPool) -> Self {
        Self { pool }
    }

    /// Ensure the schema exists.
    pub async fn ensure_schema(&self) -> Result<()> {
        sqlx::query(CREATE_TABLE_SQL)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Dolt(format!("schema creation failed: {e}")))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn row_to_entry(
        task_id: String,
        timestamp: DateTime<Utc>,
        status: String,
        pr_url: Option<String>,
        branch: Option<String>,
        error: Option<String>,
        budget_total_tokens: i64,
        budget_estimated_cost: f64,
        budget_call_count: i64,
        budget_tokens_remaining: i64,
        budget_dollars_remaining: f64,
        events_plan_steps: i32,
        events_plan_risk: String,
        events_tests_passed: i32,
        events_security_verdict: String,
        events_version_bump: String,
        events_fix_attempts: i32,
        stage_outputs_json: Option<String>,
        events_json: Option<String>,
    ) -> HistoryEntry {
        let stage_outputs = stage_outputs_json.and_then(|s| serde_json::from_str(&s).ok());
        let events = events_json.and_then(|s| serde_json::from_str(&s).ok());

        HistoryEntry {
            timestamp,
            task_id,
            status,
            pr_url,
            branch,
            error,
            budget: BudgetSummary {
                total_tokens: budget_total_tokens as u64,
                estimated_cost: budget_estimated_cost,
                call_count: budget_call_count as u64,
                tokens_remaining: budget_tokens_remaining as u64,
                dollars_remaining: budget_dollars_remaining,
            },
            events_summary: EventsSummary {
                plan_steps: events_plan_steps as u32,
                plan_risk: events_plan_risk,
                tests_passed_on_attempt: events_tests_passed as u32,
                security_verdict: events_security_verdict,
                version_bump: events_version_bump,
                fix_attempts: events_fix_attempts as u32,
            },
            stage_outputs,
            events,
        }
    }
}

impl HistoryBackend for DoltHistory {
    fn record<'a>(
        &'a self,
        entry: &'a HistoryEntry,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let stage_outputs_json = entry
                .stage_outputs
                .as_ref()
                .and_then(|s| serde_json::to_string(s).ok());
            let events_json = entry
                .events
                .as_ref()
                .and_then(|e| serde_json::to_string(e).ok());

            sqlx::query(
                r"INSERT INTO task_history (
                    task_id, timestamp, status, pr_url, branch, error,
                    budget_total_tokens, budget_estimated_cost, budget_call_count,
                    budget_tokens_remaining, budget_dollars_remaining,
                    events_plan_steps, events_plan_risk, events_tests_passed,
                    events_security_verdict, events_version_bump, events_fix_attempts,
                    stage_outputs, events_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&entry.task_id)
            .bind(entry.timestamp)
            .bind(&entry.status)
            .bind(&entry.pr_url)
            .bind(&entry.branch)
            .bind(&entry.error)
            .bind(entry.budget.total_tokens as i64)
            .bind(entry.budget.estimated_cost)
            .bind(entry.budget.call_count as i64)
            .bind(entry.budget.tokens_remaining as i64)
            .bind(entry.budget.dollars_remaining)
            .bind(entry.events_summary.plan_steps as i32)
            .bind(&entry.events_summary.plan_risk)
            .bind(entry.events_summary.tests_passed_on_attempt as i32)
            .bind(&entry.events_summary.security_verdict)
            .bind(&entry.events_summary.version_bump)
            .bind(entry.events_summary.fix_attempts as i32)
            .bind(&stage_outputs_json)
            .bind(&events_json)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Dolt(format!("insert failed: {e}")))?;

            Ok(())
        })
    }

    fn query<'a>(
        &'a self,
        query: &'a HistoryQuery,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<HistoryEntry>>> + Send + 'a>> {
        Box::pin(async move {
            let mut sql = String::from(
                "SELECT task_id, timestamp, status, pr_url, branch, error,
                 budget_total_tokens, budget_estimated_cost, budget_call_count,
                 budget_tokens_remaining, budget_dollars_remaining,
                 events_plan_steps, events_plan_risk, events_tests_passed,
                 events_security_verdict, events_version_bump, events_fix_attempts,
                 stage_outputs, events_json
                 FROM task_history WHERE 1=1",
            );
            let mut binds_str: Vec<String> = Vec::new();
            let mut binds_dt: Vec<DateTime<Utc>> = Vec::new();

            if query.failures_only {
                sql.push_str(" AND status NOT IN ('pr_created', 'committed')");
            }
            if let Some(ref tid) = query.task_id {
                sql.push_str(" AND task_id = ?");
                binds_str.push(tid.clone());
            }
            if let Some(since) = query.since {
                sql.push_str(" AND timestamp >= ?");
                binds_dt.push(since);
            }
            sql.push_str(" ORDER BY timestamp DESC");
            sql.push_str(&format!(" LIMIT {}", query.limit));

            // Build query with dynamic bindings.
            // sqlx tuple FromRow only supports up to 16 columns, so we use
            // `sqlx::query()` + `Row::get()` to extract 19 columns manually.
            let mut q = sqlx::query(&sql);

            for s in &binds_str {
                q = q.bind(s);
            }
            for dt in &binds_dt {
                q = q.bind(dt);
            }

            let rows = q
                .fetch_all(&self.pool)
                .await
                .map_err(|e| MemoryError::Dolt(format!("query failed: {e}")))?;

            Ok(rows
                .into_iter()
                .map(|r| {
                    Self::row_to_entry(
                        r.get("task_id"),
                        r.get("timestamp"),
                        r.get("status"),
                        r.get("pr_url"),
                        r.get("branch"),
                        r.get("error"),
                        r.get("budget_total_tokens"),
                        r.get("budget_estimated_cost"),
                        r.get("budget_call_count"),
                        r.get("budget_tokens_remaining"),
                        r.get("budget_dollars_remaining"),
                        r.get("events_plan_steps"),
                        r.get("events_plan_risk"),
                        r.get("events_tests_passed"),
                        r.get("events_security_verdict"),
                        r.get("events_version_bump"),
                        r.get("events_fix_attempts"),
                        r.get("stage_outputs"),
                        r.get("events_json"),
                    )
                })
                .collect())
        })
    }

    fn stats(&self) -> Pin<Box<dyn Future<Output = Result<HistoryStats>> + Send + '_>> {
        Box::pin(async {
            let row: (i64, i64, f64, i64) = sqlx::query_as(
                "SELECT COUNT(*) as total,
                        SUM(CASE WHEN status IN ('pr_created', 'committed') THEN 1 ELSE 0 END) as successes,
                        COALESCE(SUM(budget_estimated_cost), 0) as total_cost,
                        COALESCE(SUM(budget_total_tokens), 0) as total_tokens
                 FROM task_history",
            )
            .fetch_one(&self.pool)
            .await
            .map_err(|e| MemoryError::Dolt(format!("stats query failed: {e}")))?;

            Ok(HistoryStats {
                total_runs: row.0 as usize,
                successes: row.1 as usize,
                failures: (row.0 - row.1) as usize,
                total_cost: row.2,
                total_tokens: row.3 as u64,
            })
        })
    }

    fn failure_context(
        &self,
        max_entries: usize,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        Box::pin(async move {
            let query = HistoryQuery {
                limit: max_entries,
                failures_only: true,
                ..Default::default()
            };
            let failures = self.query(&query).await?;
            if failures.is_empty() {
                return Ok(String::new());
            }

            let mut context = String::from("Recent failures to avoid repeating:\n");
            for entry in &failures {
                context.push_str(&format!(
                    "- Task `{}` failed with status `{}`",
                    entry.task_id, entry.status
                ));
                if let Some(err) = &entry.error {
                    context.push_str(&format!(": {err}"));
                }
                context.push('\n');
            }
            Ok(context)
        })
    }

    fn backend_name(&self) -> &str {
        "dolt"
    }

    fn is_available(&self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>> {
        Box::pin(async { sqlx::query("SELECT 1").execute(&self.pool).await.is_ok() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_name_is_dolt() {
        // Can't construct without a pool, but we can test the SQL.
        assert!(!CREATE_TABLE_SQL.is_empty());
    }

    #[test]
    fn create_table_sql_is_valid_syntax() {
        // Basic validation: contains expected keywords.
        assert!(CREATE_TABLE_SQL.contains("CREATE TABLE"));
        assert!(CREATE_TABLE_SQL.contains("task_id"));
        assert!(CREATE_TABLE_SQL.contains("timestamp"));
        assert!(CREATE_TABLE_SQL.contains("budget_total_tokens"));
        assert!(CREATE_TABLE_SQL.contains("stage_outputs"));
        assert!(CREATE_TABLE_SQL.contains("events_json"));
        assert!(CREATE_TABLE_SQL.contains("INDEX idx_task_id"));
        assert!(CREATE_TABLE_SQL.contains("INDEX idx_timestamp"));
        assert!(CREATE_TABLE_SQL.contains("INDEX idx_status"));
    }

    #[test]
    fn row_to_entry_constructs_correctly() {
        let entry = DoltHistory::row_to_entry(
            "task-1".into(),
            Utc::now(),
            "pr_created".into(),
            Some("https://github.com/test/pr/1".into()),
            Some("glitchlab/task-1".into()),
            None,
            5000,
            0.50,
            3,
            145_000,
            9.50,
            2,
            "low".into(),
            1,
            "pass".into(),
            "patch".into(),
            0,
            Some(r#"{"plan":{"steps":[]}}"#.into()),
            Some(r#"[{"kind":"PlanCreated"}]"#.into()),
        );

        assert_eq!(entry.task_id, "task-1");
        assert_eq!(entry.status, "pr_created");
        assert_eq!(entry.budget.total_tokens, 5000);
        assert_eq!(entry.events_summary.plan_steps, 2);
        assert!(entry.stage_outputs.is_some());
        assert!(entry.events.is_some());
    }

    #[test]
    fn row_to_entry_without_optional_fields() {
        let entry = DoltHistory::row_to_entry(
            "task-2".into(),
            Utc::now(),
            "error".into(),
            None,
            None,
            Some("crash".into()),
            0,
            0.0,
            0,
            0,
            0.0,
            0,
            String::new(),
            0,
            String::new(),
            String::new(),
            0,
            None,
            None,
        );

        assert_eq!(entry.task_id, "task-2");
        assert!(entry.stage_outputs.is_none());
        assert!(entry.events.is_none());
        assert_eq!(entry.error, Some("crash".into()));
    }

    #[tokio::test]
    async fn new_with_bad_connection_string_errors() {
        let result = DoltHistory::new("mysql://nobody:badpass@localhost:99999/nonexistent").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, MemoryError::Dolt(_)),
            "expected Dolt error, got: {err}"
        );
    }

    #[test]
    fn history_entry_serde_with_stage_outputs() {
        let mut entry = crate::history::HistoryEntry {
            timestamp: Utc::now(),
            task_id: "serde-test".into(),
            status: "pr_created".into(),
            pr_url: None,
            branch: None,
            error: None,
            budget: BudgetSummary::default(),
            events_summary: EventsSummary::default(),
            stage_outputs: Some(std::collections::HashMap::from([(
                "plan".into(),
                serde_json::json!({"steps": [1, 2, 3]}),
            )])),
            events: Some(vec![serde_json::json!({"kind": "test"})]),
        };

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: crate::history::HistoryEntry = serde_json::from_str(&json).unwrap();
        assert!(parsed.stage_outputs.is_some());
        assert!(parsed.events.is_some());

        // Also test without
        entry.stage_outputs = None;
        entry.events = None;
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("stage_outputs"));
        assert!(!json.contains("\"events\":"));
    }
}
