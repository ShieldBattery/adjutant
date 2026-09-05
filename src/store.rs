use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{AssertSqlSafe, FromRow, Sqlite, SqlitePool, Transaction};
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

#[derive(Clone, Debug, Deserialize, FromRow, Serialize)]
pub struct RunLink {
    pub run_id: String,
    pub conversation_id: String,
}

#[derive(Clone, Debug, Deserialize, FromRow, Serialize)]
pub struct ProgressSnapshot {
    pub run_id: String,
    pub note: Option<String>,
    pub activity: Option<String>,
    pub note_updated_at_ms: Option<i64>,
    pub activity_updated_at_ms: Option<i64>,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, FromRow, Serialize)]
pub struct CaseRecord {
    pub run_id: String,
    pub conversation_id: String,
    pub title: String,
    pub summary: String,
    pub source_url: String,
    pub created_at_ms: i64,
    /// Whether the retained run record still exists; raw temporary artifacts may have expired.
    pub evidence_available: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StaffMessage {
    pub guild_id: u64,
    pub channel_id: u64,
    pub message_id: u64,
    pub author: String,
    pub content: String,
    pub reply_to: Option<u64>,
    pub created_at_ms: i64,
}

#[derive(FromRow)]
struct StaffMessageRow {
    guild_id: String,
    channel_id: String,
    message_id: String,
    author: String,
    content: String,
    reply_to: Option<String>,
    created_at_ms: i64,
}

impl TryFrom<StaffMessageRow> for StaffMessage {
    type Error = anyhow::Error;

    fn try_from(row: StaffMessageRow) -> Result<Self> {
        Ok(Self {
            guild_id: row
                .guild_id
                .parse()
                .context("invalid stored Discord guild ID")?,
            channel_id: row
                .channel_id
                .parse()
                .context("invalid stored Discord channel ID")?,
            message_id: row
                .message_id
                .parse()
                .context("invalid stored Discord message ID")?,
            author: row.author,
            content: row.content,
            reply_to: row
                .reply_to
                .map(|id| id.parse().context("invalid stored reply message ID"))
                .transpose()?,
            created_at_ms: row.created_at_ms,
        })
    }
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
        let now = unix_ms();
        let case_cutoff = now.saturating_sub(730_i64 * 86_400_000);
        let staff_cutoff = now.saturating_sub(90_i64 * 86_400_000);
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM message_claims WHERE claimed_at_ms < ?")
            .bind(cutoff)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM run_message_links WHERE created_at_ms < ?")
            .bind(case_cutoff)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM staff_messages WHERE created_at_ms < ?")
            .bind(staff_cutoff)
            .execute(&mut *transaction)
            .await?;
        trim_staff_messages(&mut transaction).await?;
        sqlx::query("DELETE FROM case_observations WHERE created_at_ms < ?")
            .bind(case_cutoff)
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM case_records WHERE created_at_ms < ?")
            .bind(case_cutoff)
            .execute(&mut *transaction)
            .await?;
        let result = sqlx::query("DELETE FROM runs WHERE created_at_ms < ?")
            .bind(cutoff)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(result.rows_affected())
    }

    pub async fn claim_message(&self, guild: u64, channel: u64, message: u64) -> Result<bool> {
        let result = sqlx::query("INSERT OR IGNORE INTO message_claims (guild_id, channel_id, message_id, claimed_at_ms) VALUES (?, ?, ?, ?)")
            .bind(guild.to_string()).bind(channel.to_string()).bind(message.to_string()).bind(unix_ms())
            .execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn link_message(
        &self,
        run: Uuid,
        conversation: Uuid,
        guild: u64,
        channel: u64,
        message: u64,
    ) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO run_message_links (guild_id, channel_id, message_id, run_id, conversation_id, created_at_ms) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(guild.to_string()).bind(channel.to_string()).bind(message.to_string())
            .bind(run.to_string()).bind(conversation.to_string()).bind(unix_ms())
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn message_run(
        &self,
        guild: u64,
        channel: u64,
        message: u64,
    ) -> Result<Option<RunLink>> {
        Ok(sqlx::query_as::<_, RunLink>("SELECT run_id, conversation_id FROM run_message_links WHERE guild_id = ? AND channel_id = ? AND message_id = ?")
            .bind(guild.to_string()).bind(channel.to_string()).bind(message.to_string())
            .fetch_optional(&self.pool).await?)
    }

    pub async fn conversation_runs(
        &self,
        conversation: Uuid,
        limit: usize,
    ) -> Result<Vec<RunRecord>> {
        Ok(sqlx::query_as::<_, RunRecord>("SELECT r.id, r.kind, r.title, r.request, r.bug_report_id, r.discord_guild_id, r.discord_channel_id, r.discord_message_id, r.status, r.evidence_manifest, r.final_report, r.error, r.created_at_ms, r.started_at_ms, r.finished_at_ms FROM runs r JOIN (SELECT DISTINCT run_id FROM run_message_links WHERE conversation_id = ?) links ON links.run_id = r.id ORDER BY r.created_at_ms DESC, r.id DESC LIMIT ?")
            .bind(conversation.to_string()).bind(bounded_limit(limit, 10)).fetch_all(&self.pool).await?)
    }

    pub async fn active_runs(&self, limit: usize) -> Result<Vec<RunRecord>> {
        Ok(sqlx::query_as::<_, RunRecord>("SELECT id, kind, title, request, bug_report_id, discord_guild_id, discord_channel_id, discord_message_id, status, evidence_manifest, final_report, error, created_at_ms, started_at_ms, finished_at_ms FROM runs WHERE status IN ('queued', 'running') ORDER BY CASE status WHEN 'running' THEN 0 ELSE 1 END, created_at_ms ASC, id ASC LIMIT ?")
            .bind(bounded_limit(limit, 20)).fetch_all(&self.pool).await?)
    }

    pub async fn set_progress(
        &self,
        run: Uuid,
        note: Option<&str>,
        activity: Option<&str>,
    ) -> Result<()> {
        let note = note.map(|value| truncate_chars(value, 1_000));
        let activity = activity.map(|value| truncate_chars(value, 200));
        let run_id = run.to_string();
        let updated_at_ms = unix_ms();
        let note_updated_at_ms = note.as_ref().map(|_| updated_at_ms);
        let activity_updated_at_ms = activity.as_ref().map(|_| updated_at_ms);
        sqlx::query("INSERT INTO run_progress (run_id, note, activity, note_updated_at_ms, activity_updated_at_ms, updated_at_ms) SELECT ?, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM runs WHERE id = ? AND status IN ('queued', 'running')) ON CONFLICT(run_id) DO UPDATE SET note = COALESCE(excluded.note, run_progress.note), activity = COALESCE(excluded.activity, run_progress.activity), note_updated_at_ms = COALESCE(excluded.note_updated_at_ms, run_progress.note_updated_at_ms), activity_updated_at_ms = COALESCE(excluded.activity_updated_at_ms, run_progress.activity_updated_at_ms), updated_at_ms = excluded.updated_at_ms WHERE EXISTS (SELECT 1 FROM runs WHERE id = excluded.run_id AND status IN ('queued', 'running'))")
            .bind(&run_id)
            .bind(note)
            .bind(activity)
            .bind(note_updated_at_ms)
            .bind(activity_updated_at_ms)
            .bind(updated_at_ms)
            .bind(run_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_progress(&self, run: Uuid) -> Result<Option<ProgressSnapshot>> {
        Ok(sqlx::query_as::<_, ProgressSnapshot>(
            "SELECT run_id, note, activity, note_updated_at_ms, activity_updated_at_ms, updated_at_ms FROM run_progress WHERE run_id = ?",
        )
        .bind(run.to_string())
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn save_case(
        &self,
        run: Uuid,
        conversation: Uuid,
        title: &str,
        summary: &str,
        source_url: &str,
    ) -> Result<()> {
        sqlx::query("INSERT INTO case_records (run_id, conversation_id, title, summary, source_url, created_at_ms) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(run_id) DO NOTHING")
            .bind(run.to_string()).bind(conversation.to_string())
            .bind(truncate_chars(title, 200)).bind(truncate_chars(summary, 12_000))
            .bind(truncate_chars(source_url, 300)).bind(unix_ms())
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn search_cases(&self, query: &str, limit: usize) -> Result<Vec<CaseRecord>> {
        let terms = search_terms(query);
        let mut sql = String::from(
            "SELECT c.run_id, c.conversation_id, c.title, c.summary, c.source_url, c.created_at_ms, EXISTS(SELECT 1 FROM runs r WHERE r.id = c.run_id) AS evidence_available FROM case_records c",
        );
        if !terms.is_empty() {
            sql.push_str(" WHERE ");
            for (index, _) in terms.iter().enumerate() {
                if index > 0 {
                    sql.push_str(" AND ");
                }
                sql.push_str("(c.title LIKE ? ESCAPE '\\' OR c.summary LIKE ? ESCAPE '\\' OR c.source_url LIKE ? ESCAPE '\\')");
            }
        }
        sql.push_str(" ORDER BY c.created_at_ms DESC LIMIT ?");
        // The generated SQL contains only fixed clauses; user terms are always bound below.
        let mut statement = sqlx::query_as::<_, CaseRecord>(AssertSqlSafe(sql));
        for term in terms {
            let pattern = format!("%{}%", escape_like(&term));
            statement = statement
                .bind(pattern.clone())
                .bind(pattern.clone())
                .bind(pattern);
        }
        Ok(statement
            .bind(bounded_limit(limit, 20))
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn case_history(&self, conversation: Uuid, limit: usize) -> Result<Vec<CaseRecord>> {
        Ok(sqlx::query_as::<_, CaseRecord>("SELECT c.run_id, c.conversation_id, c.title, c.summary, c.source_url, c.created_at_ms, EXISTS(SELECT 1 FROM runs r WHERE r.id = c.run_id) AS evidence_available FROM case_records c WHERE c.conversation_id = ? ORDER BY c.created_at_ms DESC LIMIT ?")
            .bind(conversation.to_string()).bind(bounded_limit(limit, 10)).fetch_all(&self.pool).await?)
    }

    pub async fn store_staff_message(&self, message: &StaffMessage) -> Result<()> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("INSERT INTO staff_messages (guild_id, channel_id, message_id, author, content, reply_to, created_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(guild_id, channel_id, message_id) DO UPDATE SET author = excluded.author, content = excluded.content, reply_to = excluded.reply_to, created_at_ms = excluded.created_at_ms")
            .bind(message.guild_id.to_string()).bind(message.channel_id.to_string()).bind(message.message_id.to_string())
            .bind(truncate_chars(&message.author, 100)).bind(truncate_chars(&message.content, 8_000))
            .bind(message.reply_to.map(|id| id.to_string())).bind(message.created_at_ms)
            .execute(&mut *transaction).await?;
        sqlx::query("DELETE FROM staff_messages WHERE created_at_ms < ?")
            .bind(unix_ms().saturating_sub(90_i64 * 86_400_000))
            .execute(&mut *transaction)
            .await?;
        trim_staff_messages(&mut transaction).await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn recent_staff_messages(
        &self,
        guild: u64,
        channel: u64,
        limit: usize,
    ) -> Result<Vec<StaffMessage>> {
        let rows = sqlx::query_as::<_, StaffMessageRow>("SELECT guild_id, channel_id, message_id, author, content, reply_to, created_at_ms FROM staff_messages WHERE guild_id = ? AND channel_id = ? ORDER BY created_at_ms DESC, message_id DESC LIMIT ?")
            .bind(guild.to_string()).bind(channel.to_string()).bind(bounded_limit(limit, 30))
            .fetch_all(&self.pool).await?;
        rows.into_iter().map(StaffMessage::try_from).collect()
    }

    pub async fn search_staff_messages(
        &self,
        guild: u64,
        channel: u64,
        query: &str,
        limit: usize,
    ) -> Result<Vec<StaffMessage>> {
        let terms = search_terms(query);
        let mut sql = String::from(
            "SELECT guild_id, channel_id, message_id, author, content, reply_to, created_at_ms FROM staff_messages WHERE guild_id = ? AND channel_id = ?",
        );
        for _ in &terms {
            sql.push_str(" AND (author LIKE ? ESCAPE '\\' OR content LIKE ? ESCAPE '\\')");
        }
        sql.push_str(" ORDER BY created_at_ms DESC, message_id DESC LIMIT ?");
        // The generated SQL contains only fixed clauses; user terms are always bound below.
        let mut statement = sqlx::query_as::<_, StaffMessageRow>(AssertSqlSafe(sql))
            .bind(guild.to_string())
            .bind(channel.to_string());
        for term in terms {
            let pattern = format!("%{}%", escape_like(&term));
            statement = statement.bind(pattern.clone()).bind(pattern);
        }
        let rows = statement
            .bind(bounded_limit(limit, 30))
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(StaffMessage::try_from).collect()
    }

    pub async fn delete_staff_message(&self, guild: u64, channel: u64, message: u64) -> Result<()> {
        sqlx::query(
            "DELETE FROM staff_messages WHERE guild_id = ? AND channel_id = ? AND message_id = ?",
        )
        .bind(guild.to_string())
        .bind(channel.to_string())
        .bind(message.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn save_case_observation(
        &self,
        conversation: Uuid,
        source_url: &str,
        text: &str,
    ) -> Result<()> {
        sqlx::query("INSERT INTO case_observations (conversation_id, source_url, text, created_at_ms) VALUES (?, ?, ?, ?) ON CONFLICT(conversation_id, source_url) DO NOTHING")
            .bind(conversation.to_string()).bind(truncate_chars(source_url, 300)).bind(truncate_chars(text, 4_000)).bind(unix_ms())
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn case_observations(&self, conversation: Uuid, limit: usize) -> Result<Vec<Value>> {
        let rows: Vec<(String, String, i64)> = sqlx::query_as("SELECT source_url, text, created_at_ms FROM case_observations WHERE conversation_id = ? ORDER BY created_at_ms DESC LIMIT ?")
            .bind(conversation.to_string()).bind(bounded_limit(limit, 20)).fetch_all(&self.pool).await?;
        Ok(rows.into_iter().map(|(source_url, text, created_at_ms)| json!({
            "kind": "unverified_staff_statement", "source_url": source_url, "text": text, "created_at_ms": created_at_ms,
        })).collect())
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

async fn trim_staff_messages(transaction: &mut Transaction<'_, Sqlite>) -> Result<()> {
    sqlx::query("DELETE FROM staff_messages WHERE (guild_id, channel_id, message_id) IN (SELECT guild_id, channel_id, message_id FROM staff_messages ORDER BY created_at_ms DESC, guild_id DESC, channel_id DESC, message_id DESC LIMIT -1 OFFSET 10000)")
        .execute(&mut **transaction)
        .await?;
    Ok(())
}

fn bounded_limit(limit: usize, maximum: usize) -> i64 {
    i64::try_from(limit.min(maximum)).unwrap_or(i64::MAX)
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}

fn search_terms(query: &str) -> Vec<String> {
    truncate_chars(query, 200)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
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

    #[tokio::test]
    async fn active_status_does_not_hide_running_work_behind_queued_requests() {
        let (_directory, store) = test_store().await;
        let queued = new_run();
        let running = new_run();
        store.create_run(&queued).await.unwrap();
        store.create_run(&running).await.unwrap();
        store.mark_running(running.id).await.unwrap();
        let visible = store.active_runs(1).await.unwrap();
        assert_eq!(visible[0].id, running.id.to_string());
    }

    #[tokio::test]
    async fn claims_messages_and_keeps_conversation_context_isolated() {
        let (_directory, store) = test_store().await;
        assert!(store.claim_message(1, 2, 3).await.unwrap());
        assert!(!store.claim_message(1, 2, 3).await.unwrap());
        assert!(store.claim_message(2, 2, 3).await.unwrap());

        let first = new_run();
        let second = new_run();
        store.create_run(&first).await.unwrap();
        store.create_run(&second).await.unwrap();
        let first_conversation = Uuid::now_v7();
        let second_conversation = Uuid::now_v7();
        store
            .link_message(first.id, first_conversation, 1, 2, 10)
            .await
            .unwrap();
        store
            .link_message(first.id, first_conversation, 1, 2, 11)
            .await
            .unwrap();
        store
            .link_message(second.id, second_conversation, 1, 2, 12)
            .await
            .unwrap();
        // A later conversational reply must not make the older run sort newer.
        store
            .link_message(second.id, first_conversation, 1, 2, 13)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET created_at_ms = ? WHERE id = ?")
            .bind(10_i64)
            .bind(first.id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET created_at_ms = ? WHERE id = ?")
            .bind(20_i64)
            .bind(second.id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE run_message_links SET created_at_ms = ? WHERE run_id = ?")
            .bind(30_i64)
            .bind(first.id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        // A Discord message keeps its first immutable mapping.
        store
            .link_message(second.id, second_conversation, 1, 2, 10)
            .await
            .unwrap();

        let link = store.message_run(1, 2, 10).await.unwrap().unwrap();
        assert_eq!(link.run_id, first.id.to_string());
        assert_eq!(link.conversation_id, first_conversation.to_string());
        assert!(store.message_run(9, 2, 10).await.unwrap().is_none());
        let first_runs = store
            .conversation_runs(first_conversation, 99)
            .await
            .unwrap();
        assert_eq!(first_runs.len(), 2);
        assert_eq!(first_runs[0].id, second.id.to_string());
        assert_eq!(first_runs[1].id, first.id.to_string());
        assert_eq!(
            store
                .conversation_runs(second_conversation, 99)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(store.active_runs(99).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn caps_progress_and_staff_context_without_splitting_utf8() {
        let (_directory, store) = test_store().await;
        let run = new_run();
        store.create_run(&run).await.unwrap();
        store
            .set_progress(run.id, Some(&"é".repeat(1_200)), None)
            .await
            .unwrap();
        let after_note = store.get_progress(run.id).await.unwrap().unwrap();
        store
            .set_progress(run.id, None, Some(&"🦀".repeat(300)))
            .await
            .unwrap();
        let progress = store.get_progress(run.id).await.unwrap().unwrap();
        assert_eq!(progress.note.unwrap().chars().count(), 1_000);
        assert_eq!(progress.note_updated_at_ms, after_note.note_updated_at_ms);
        assert!(progress.activity_updated_at_ms.is_some());
        assert_eq!(progress.activity.unwrap().chars().count(), 200);
        store.complete_run(run.id, "done").await.unwrap();
        store
            .set_progress(run.id, Some("late"), Some("late"))
            .await
            .unwrap();
        assert_ne!(
            store
                .get_progress(run.id)
                .await
                .unwrap()
                .unwrap()
                .note
                .as_deref(),
            Some("late")
        );

        let message = StaffMessage {
            guild_id: 1,
            channel_id: 2,
            message_id: 3,
            author: "é".repeat(120),
            content: "🦀".repeat(8_100),
            reply_to: Some(4),
            created_at_ms: unix_ms(),
        };
        store.store_staff_message(&message).await.unwrap();
        let stored = store.recent_staff_messages(1, 2, 99).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].author.chars().count(), 100);
        assert_eq!(stored[0].content.chars().count(), 8_000);
        assert_eq!(stored[0].reply_to, Some(4));

        let conversation = Uuid::now_v7();
        let source_url = format!("https://example.invalid/{}", "x".repeat(400));
        store
            .save_case(
                run.id,
                conversation,
                &"é".repeat(250),
                &"🦀".repeat(12_100),
                &source_url,
            )
            .await
            .unwrap();
        let case = store
            .case_history(conversation, 99)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(case.title.chars().count(), 200);
        assert_eq!(case.summary.chars().count(), 12_000);
        assert_eq!(case.source_url.chars().count(), 300);
        store
            .save_case_observation(conversation, &source_url, &"é".repeat(4_100))
            .await
            .unwrap();
        assert_eq!(
            store.case_observations(conversation, 99).await.unwrap()[0]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            4_000
        );
    }

    #[tokio::test]
    async fn literal_search_does_not_treat_wildcards_as_patterns() {
        let (_directory, store) = test_store().await;
        let conversation = Uuid::now_v7();
        let matching = new_run();
        let other = new_run();
        store.create_run(&matching).await.unwrap();
        store.create_run(&other).await.unwrap();
        store
            .save_case(
                matching.id,
                conversation,
                "literal %_\\ value",
                "summary",
                "https://example.invalid/a",
            )
            .await
            .unwrap();
        store
            .save_case(
                other.id,
                conversation,
                "literal xa value",
                "summary",
                "https://example.invalid/b",
            )
            .await
            .unwrap();
        let cases = store.search_cases("%_\\", 20).await.unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].run_id, matching.id.to_string());

        for (id, content) in [(10, "literal %_\\ value"), (11, "literal xa value")] {
            store
                .store_staff_message(&StaffMessage {
                    guild_id: 1,
                    channel_id: 2,
                    message_id: id,
                    author: "staff".to_owned(),
                    content: content.to_owned(),
                    reply_to: None,
                    created_at_ms: unix_ms(),
                })
                .await
                .unwrap();
        }
        assert_eq!(
            store
                .search_staff_messages(1, 2, "%_\\", 30)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn cases_and_observations_are_immutable_and_outlive_pruned_evidence() {
        let (_directory, store) = test_store().await;
        let conversation = Uuid::now_v7();
        let run = new_run();
        store.create_run(&run).await.unwrap();
        store
            .link_message(run.id, conversation, 1, 2, 99)
            .await
            .unwrap();
        store
            .save_case(
                run.id,
                conversation,
                "original",
                "first summary",
                "https://example.invalid/case",
            )
            .await
            .unwrap();
        store
            .save_case(
                run.id,
                Uuid::now_v7(),
                "replacement",
                "second summary",
                "https://example.invalid/other",
            )
            .await
            .unwrap();
        store
            .save_case_observation(
                conversation,
                "https://discord.invalid/messages/1",
                "first correction",
            )
            .await
            .unwrap();
        store
            .save_case_observation(
                conversation,
                "https://discord.invalid/messages/1",
                "replacement correction",
            )
            .await
            .unwrap();

        let initial = store.case_history(conversation, 20).await.unwrap();
        assert_eq!(initial[0].title, "original");
        assert!(initial[0].evidence_available);
        let observations = store.case_observations(conversation, 99).await.unwrap();
        assert_eq!(observations[0]["kind"], "unverified_staff_statement");
        assert_eq!(observations[0]["text"], "first correction");

        sqlx::query("UPDATE runs SET created_at_ms = 0 WHERE id = ?")
            .bind(run.id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.prune(1).await.unwrap(), 1);
        let retained = store.case_history(conversation, 20).await.unwrap();
        assert_eq!(retained.len(), 1);
        assert!(!retained[0].evidence_available);
        assert_eq!(
            store
                .message_run(1, 2, 99)
                .await
                .unwrap()
                .unwrap()
                .conversation_id,
            conversation.to_string()
        );
        assert_eq!(
            store
                .case_observations(conversation, 20)
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
