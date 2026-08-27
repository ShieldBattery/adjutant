use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    BugReport,
    StaffRequest,
}

impl RunKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::BugReport => "bug_report",
            Self::StaffRequest => "staff_request",
        }
    }
}

#[derive(Debug)]
pub struct NewRun {
    pub id: Uuid,
    pub kind: RunKind,
    pub title: String,
    pub request: String,
    pub bug_report_id: Option<Uuid>,
    pub discord_guild_id: u64,
    pub discord_channel_id: u64,
    pub discord_message_id: u64,
}

impl NewRun {
    #[must_use]
    pub fn new(
        kind: RunKind,
        title: String,
        request: String,
        bug_report_id: Option<Uuid>,
        discord_guild_id: u64,
        discord_channel_id: u64,
        discord_message_id: u64,
    ) -> Self {
        Self {
            id: Uuid::now_v7(),
            kind,
            title,
            request,
            bug_report_id,
            discord_guild_id,
            discord_channel_id,
            discord_message_id,
        }
    }
}

#[derive(Clone, Debug, FromRow)]
pub struct RunRecord {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub request: String,
    pub bug_report_id: Option<String>,
    pub discord_guild_id: String,
    pub discord_channel_id: String,
    pub discord_message_id: String,
    pub status: String,
    pub evidence_manifest: Option<String>,
    pub final_report: Option<String>,
    pub error: Option<String>,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

#[derive(Clone, Debug, FromRow)]
pub struct RunEvent {
    pub seq: i64,
    pub occurred_at_ms: i64,
    pub kind: String,
    pub event_json: String,
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("failed to create database directory {}", parent.display())
            })?;
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .with_context(|| format!("failed to open database {}", path.display()))?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("failed to migrate run database")?;

        Ok(Self { pool })
    }

    pub async fn recover_interrupted_runs(&self) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE runs SET status = 'failed', error = ?, finished_at_ms = ? WHERE status IN ('queued', 'running')",
        )
        .bind("Adjutant restarted before this run completed")
        .bind(unix_ms())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn prune(&self, retention_days: u64) -> Result<u64> {
        let retention_ms = i64::try_from(retention_days)?.saturating_mul(86_400_000);
        let cutoff = unix_ms().saturating_sub(retention_ms);
        let result = sqlx::query("DELETE FROM runs WHERE created_at_ms < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    pub async fn create_run(&self, run: &NewRun) -> Result<()> {
        sqlx::query(
            "INSERT INTO runs (id, kind, title, request, bug_report_id, discord_guild_id, discord_channel_id, discord_message_id, status, created_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'queued', ?)",
        )
        .bind(run.id.to_string())
        .bind(run.kind.as_str())
        .bind(&run.title)
        .bind(&run.request)
        .bind(run.bug_report_id.map(|id| id.to_string()))
        .bind(run.discord_guild_id.to_string())
        .bind(run.discord_channel_id.to_string())
        .bind(run.discord_message_id.to_string())
        .bind(unix_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_running(&self, id: Uuid) -> Result<()> {
        sqlx::query("UPDATE runs SET status = 'running', started_at_ms = ? WHERE id = ?")
            .bind(unix_ms())
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_evidence_manifest(&self, id: Uuid, manifest: &str) -> Result<()> {
        sqlx::query("UPDATE runs SET evidence_manifest = ? WHERE id = ?")
            .bind(manifest)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn complete_run(&self, id: Uuid, report: &str) -> Result<()> {
        sqlx::query(
            "UPDATE runs SET status = 'succeeded', final_report = ?, error = NULL, finished_at_ms = ? WHERE id = ?",
        )
        .bind(report)
        .bind(unix_ms())
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn fail_run(&self, id: Uuid, error: &str) -> Result<()> {
        sqlx::query(
            "UPDATE runs SET status = 'failed', error = ?, finished_at_ms = ? WHERE id = ?",
        )
        .bind(error)
        .bind(unix_ms())
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn append_event(&self, run_id: Uuid, event_json: &str) -> Result<i64> {
        let value: Value = serde_json::from_str(event_json).context("invalid Codex JSONL event")?;
        let kind = event_kind(&value);
        let mut transaction = self.pool.begin().await?;
        let (seq,): (i64,) = sqlx::query_as(
            "UPDATE runs SET next_event_seq = next_event_seq + 1 WHERE id = ? RETURNING next_event_seq - 1",
        )
        .bind(run_id.to_string())
        .fetch_one(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO run_events (run_id, seq, occurred_at_ms, kind, event_json) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(run_id.to_string())
        .bind(seq)
        .bind(unix_ms())
        .bind(kind)
        .bind(event_json)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(seq)
    }

    pub async fn list_runs(&self, limit: i64) -> Result<Vec<RunRecord>> {
        Ok(sqlx::query_as::<_, RunRecord>(
            "SELECT id, kind, title, request, bug_report_id, discord_guild_id, discord_channel_id, discord_message_id, status, evidence_manifest, final_report, error, created_at_ms, started_at_ms, finished_at_ms FROM runs ORDER BY created_at_ms DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn get_run(&self, id: &str) -> Result<Option<RunRecord>> {
        Ok(sqlx::query_as::<_, RunRecord>(
            "SELECT id, kind, title, request, bug_report_id, discord_guild_id, discord_channel_id, discord_message_id, status, evidence_manifest, final_report, error, created_at_ms, started_at_ms, finished_at_ms FROM runs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn get_events(&self, run_id: &str) -> Result<Vec<RunEvent>> {
        Ok(sqlx::query_as::<_, RunEvent>(
            "SELECT seq, occurred_at_ms, kind, event_json FROM run_events WHERE run_id = ? ORDER BY seq",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

fn event_kind(value: &Value) -> String {
    let outer = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let item = value
        .get("item")
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str);
    item.map_or_else(|| outer.to_owned(), |item| format!("{outer}.{item}"))
}

#[must_use]
pub fn unix_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("runs.sqlite3"))
            .await
            .unwrap();
        (directory, store)
    }

    fn new_run() -> NewRun {
        NewRun::new(
            RunKind::StaffRequest,
            "Investigate latency".to_owned(),
            "What happened to this game?".to_owned(),
            None,
            1,
            2,
            3,
        )
    }

    #[tokio::test]
    async fn stores_run_lifecycle_and_events() {
        let (_directory, store) = test_store().await;
        let run = new_run();
        store.create_run(&run).await.unwrap();
        store.mark_running(run.id).await.unwrap();
        assert_eq!(
            store
                .append_event(
                    run.id,
                    r#"{"type":"item.completed","item":{"type":"mcp_tool_call"}}"#
                )
                .await
                .unwrap(),
            0
        );
        store
            .complete_run(run.id, "Root cause found")
            .await
            .unwrap();

        let stored = store.get_run(&run.id.to_string()).await.unwrap().unwrap();
        assert_eq!(stored.status, "succeeded");
        assert_eq!(stored.final_report.as_deref(), Some("Root cause found"));
        let events = store.get_events(&run.id.to_string()).await.unwrap();
        assert_eq!(events[0].kind, "item.completed.mcp_tool_call");
    }

    #[tokio::test]
    async fn recovers_incomplete_runs() {
        let (_directory, store) = test_store().await;
        let run = new_run();
        store.create_run(&run).await.unwrap();

        assert_eq!(store.recover_interrupted_runs().await.unwrap(), 1);
        let stored = store.get_run(&run.id.to_string()).await.unwrap().unwrap();
        assert_eq!(stored.status, "failed");
        assert!(stored.error.unwrap().contains("restarted"));
    }
}
