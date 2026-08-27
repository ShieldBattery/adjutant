use std::{
    fmt::Write,
    ops::ControlFlow,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use futures_util::TryStreamExt;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlparser::{
    ast::{Query, Select, Statement, Visit, Visitor},
    dialect::PostgreSqlDialect,
    parser::Parser,
    tokenizer::{Token, Tokenizer},
};
use sqlx::{AssertSqlSafe, PgPool, postgres::PgPoolOptions, types::Json};
use tracing::{info, warn};

use crate::config::Config;

/// A connected, bounded PostgreSQL reader.
#[derive(Debug)]
pub struct Database {
    pool: PgPool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct QueryResult {
    pub rows: Vec<Value>,
    pub row_count: usize,
    pub truncated: bool,
    /// Rows skipped because their JSON representation exceeded the configured
    /// per-row limit before crossing the PostgreSQL wire protocol.
    pub oversized_row_count: usize,
    pub response_bytes: usize,
    pub duration_ms: u128,
    pub query_sha256: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SchemaResult {
    pub columns: Vec<SchemaColumn>,
    pub truncated: bool,
    pub duration_ms: u128,
}

#[derive(Debug, Serialize, JsonSchema, sqlx::FromRow)]
pub struct SchemaColumn {
    pub table_schema: String,
    pub table_name: String,
    pub table_type: String,
    pub column_name: String,
    pub data_type: String,
    pub is_nullable: String,
    pub ordinal_position: i32,
}

enum DatabaseEnvelope {
    Row(Value),
    Oversized,
}

fn decode_database_envelope(envelope: &Value) -> Result<DatabaseEnvelope> {
    let object = envelope
        .as_object()
        .context("database returned an invalid diagnostic row envelope")?;
    if object
        .get("_adjutant_oversized")
        .is_some_and(Value::is_boolean)
    {
        return Ok(DatabaseEnvelope::Oversized);
    }
    object
        .get("_adjutant_row")
        .cloned()
        .map(DatabaseEnvelope::Row)
        .context("database returned an invalid diagnostic row envelope")
}

impl Database {
    pub async fn connect(config: &Config) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(Duration::from_secs(5))
            .connect(&config.database_url)
            .await
            .context("failed to connect to the diagnostic database")?;
        Ok(Self { pool })
    }

    pub async fn healthcheck(&self) -> Result<()> {
        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .context("diagnostic database health check failed")?;
        Ok(())
    }

    pub async fn schema(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
        config: &Config,
    ) -> Result<SchemaResult> {
        let started = Instant::now();
        let mut transaction = self.read_only_transaction(config).await?;
        let rows = sqlx::query_as::<_, SchemaColumn>(
            r"
            SELECT c.table_schema, c.table_name, t.table_type, c.column_name,
                   c.data_type, c.is_nullable, c.ordinal_position
            FROM information_schema.columns AS c
            JOIN information_schema.tables AS t
              ON t.table_catalog = c.table_catalog
             AND t.table_schema = c.table_schema
             AND t.table_name = c.table_name
            WHERE c.table_schema NOT IN ('information_schema', 'pg_catalog')
              AND ($1::text IS NULL OR c.table_schema = $1)
              AND ($2::text IS NULL OR c.table_name = $2)
            ORDER BY c.table_schema, c.table_name, c.ordinal_position
            LIMIT $3
            ",
        )
        .bind(schema)
        .bind(table)
        .bind(i64::from(config.max_rows) + 1)
        .fetch_all(&mut *transaction)
        .await
        .context("schema discovery query failed")?;
        transaction
            .rollback()
            .await
            .context("failed to close schema transaction")?;

        let truncated = rows.len() > config.max_rows as usize;
        let columns = rows.into_iter().take(config.max_rows as usize).collect();
        Ok(SchemaResult {
            columns,
            truncated,
            duration_ms: started.elapsed().as_millis(),
        })
    }

    pub async fn query(
        &self,
        sql: &str,
        requested_max_rows: Option<u32>,
        config: &Config,
    ) -> Result<QueryResult> {
        validate_query(sql, config.max_sql_bytes)?;
        let query_sha256 = query_hash(sql);
        let row_cap = requested_max_rows
            .unwrap_or(config.max_rows)
            .min(config.max_rows);
        if row_cap == 0 {
            bail!("max_rows must be greater than zero");
        }

        let started = Instant::now();
        let result = self.query_inner(sql, row_cap, config, &query_sha256).await;
        match &result {
            Ok(result) => info!(
                query_sha256,
                duration_ms = result.duration_ms,
                rows = result.row_count,
                truncated = result.truncated,
                response_bytes = result.response_bytes,
                "completed database diagnostic query"
            ),
            Err(error) => warn!(
                query_sha256, duration_ms = started.elapsed().as_millis(), error = %error,
                "database diagnostic query failed"
            ),
        }
        result
    }

    async fn query_inner(
        &self,
        sql: &str,
        row_cap: u32,
        config: &Config,
        query_sha256: &str,
    ) -> Result<QueryResult> {
        let started = Instant::now();
        let mut transaction = self.read_only_transaction(config).await?;
        let wrapped = format!(
            "SELECT CASE \
                WHEN octet_length(encoded.row_json::text) <= $1 THEN \
                    jsonb_build_object('_adjutant_row', encoded.row_json) \
                ELSE jsonb_build_object( \
                    '_adjutant_oversized', true, \
                    '_adjutant_row_bytes', octet_length(encoded.row_json::text) \
                ) \
            END AS row_json \
            FROM ({sql}) AS q \
            CROSS JOIN LATERAL (SELECT to_jsonb(q) AS row_json) AS encoded \
            LIMIT $2"
        );
        // `validate_query` has parsed the interpolated query and rejected every
        // non-query statement before this audited dynamic SQL boundary.
        let result = async {
            let mut encoded_rows = sqlx::query_scalar::<_, Json<Value>>(AssertSqlSafe(wrapped))
                .bind(i64::try_from(config.max_row_bytes).context("row limit is too large")?)
                .bind(i64::from(row_cap) + 1)
                .fetch(&mut *transaction);

            let mut rows = Vec::with_capacity(row_cap as usize);
            let mut response_bytes = 0_usize;
            let mut truncated = false;
            let mut oversized_row_count = 0_usize;
            let mut source_row_count = 0_u32;
            while let Some(Json(envelope)) = encoded_rows
                .try_next()
                .await
                .context("read-only query execution failed")?
            {
                source_row_count += 1;
                if source_row_count > row_cap {
                    truncated = true;
                    break;
                }
                match decode_database_envelope(&envelope)? {
                    DatabaseEnvelope::Oversized => {
                        oversized_row_count += 1;
                        truncated = true;
                    }
                    DatabaseEnvelope::Row(row) => {
                        let row_bytes = serde_json::to_vec(&row)
                            .context("failed to encode database row")?
                            .len();
                        if row_bytes > config.max_row_bytes {
                            bail!("database row exceeded its configured size limit");
                        }
                        if response_bytes.saturating_add(row_bytes) > config.max_response_bytes {
                            truncated = true;
                            break;
                        }
                        response_bytes += row_bytes;
                        rows.push(row);
                    }
                }
            }

            Ok::<_, anyhow::Error>((rows, response_bytes, truncated, oversized_row_count))
        }
        .await;
        transaction
            .rollback()
            .await
            .context("failed to close query transaction")?;
        let (rows, response_bytes, truncated, oversized_row_count) = result?;

        Ok(QueryResult {
            row_count: rows.len(),
            rows,
            truncated,
            oversized_row_count,
            response_bytes,
            duration_ms: started.elapsed().as_millis(),
            query_sha256: query_sha256.to_owned(),
        })
    }

    async fn read_only_transaction(
        &self,
        config: &Config,
    ) -> Result<sqlx::Transaction<'_, sqlx::Postgres>> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .context("failed to begin database transaction")?;
        sqlx::query("SET TRANSACTION READ ONLY")
            .execute(&mut *transaction)
            .await
            .context("failed to make database transaction read-only")?;
        sqlx::query("SELECT set_config('statement_timeout', $1, true)")
            .bind(format!("{}ms", config.statement_timeout.as_millis()))
            .execute(&mut *transaction)
            .await
            .context("failed to set statement timeout")?;
        sqlx::query("SELECT set_config('lock_timeout', $1, true)")
            .bind(format!("{}ms", config.lock_timeout.as_millis()))
            .execute(&mut *transaction)
            .await
            .context("failed to set lock timeout")?;
        Ok(transaction)
    }
}

/// Validates the deliberately small query surface accepted by this server.
pub fn validate_query(sql: &str, max_sql_bytes: usize) -> Result<()> {
    if sql.trim().is_empty() {
        bail!("query must not be empty");
    }
    if sql.len() > max_sql_bytes {
        bail!("query exceeds the {max_sql_bytes}-byte limit");
    }

    let dialect = PostgreSqlDialect {};
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize()
        .context("query could not be tokenized")?;
    if tokens
        .iter()
        .any(|token| matches!(token, Token::Placeholder(_)))
    {
        bail!("query placeholders are not supported");
    }
    if tokens.iter().any(|token| matches!(token, Token::SemiColon)) {
        bail!("semicolon statement terminators are not supported");
    }
    if contains_advisory_lock_call(&tokens) {
        bail!("session-level advisory lock functions are not allowed");
    }
    // sqlparser's PostgreSQL dialect does not currently parse the PostgreSQL
    // `TABLE relation` shorthand. Validate it as its equivalent SELECT while
    // still executing the original, standards-supported PostgreSQL query.
    let parser_input = table_shorthand_as_select(sql);
    let statements =
        Parser::parse_sql(&dialect, &parser_input).context("query could not be parsed")?;
    let [statement] = statements.as_slice() else {
        bail!("exactly one SQL statement is required");
    };
    if !matches!(statement, Statement::Query(_)) {
        bail!("only SELECT, WITH, VALUES, and TABLE queries are allowed");
    }

    let mut safety = QuerySafetyVisitor;
    if let ControlFlow::Break(reason) = statement.visit(&mut safety) {
        bail!("query is not read-only: {reason}");
    }
    Ok(())
}

fn contains_advisory_lock_call(tokens: &[Token]) -> bool {
    let significant: Vec<_> = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect();
    significant.windows(2).any(|tokens| {
        let [Token::Word(function_name), Token::LParen] = tokens else {
            return false;
        };
        let name = function_name.value.to_ascii_lowercase();
        name.starts_with("pg_advisory_") || name.starts_with("pg_try_advisory_")
    })
}

fn table_shorthand_as_select(sql: &str) -> String {
    let trimmed = sql.trim_start();
    let Some(keyword) = trimmed.get(..5) else {
        return sql.to_owned();
    };
    if !keyword.eq_ignore_ascii_case("table") {
        return sql.to_owned();
    }
    let remainder = &trimmed[5..];
    if !remainder.starts_with(char::is_whitespace) {
        return sql.to_owned();
    }
    format!("SELECT * FROM {remainder}")
}

fn query_hash(sql: &str) -> String {
    let digest = Sha256::digest(sql.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

struct QuerySafetyVisitor;

impl Visitor for QuerySafetyVisitor {
    type Break = &'static str;

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        if !query.locks.is_empty() || query.for_clause.is_some() {
            return ControlFlow::Break("locking clauses are not allowed");
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<Self::Break> {
        if select.into.is_some() {
            return ControlFlow::Break("SELECT INTO is not allowed");
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
        if !matches!(statement, Statement::Query(_)) {
            return ControlFlow::Break("data-modifying statement found inside query");
        }
        ControlFlow::Continue(())
    }
}

#[cfg(test)]
mod tests {
    use super::validate_query;

    const LIMIT: usize = 1024;

    #[test]
    fn allows_read_only_queries() {
        for query in [
            "SELECT 1",
            "VALUES (1), (2)",
            "TABLE public.users",
            "WITH latest AS (SELECT 1 AS id) SELECT * FROM latest",
            "SELECT * FROM users WHERE id IN (SELECT user_id FROM games_users)",
            "SELECT 1 UNION ALL SELECT 2",
        ] {
            validate_query(query, LIMIT).unwrap_or_else(|error| panic!("{query}: {error}"));
        }
    }

    #[test]
    fn rejects_writes_and_locks_even_when_nested() {
        for query in [
            "INSERT INTO users (name) VALUES ('x')",
            "WITH changed AS (DELETE FROM users RETURNING id) SELECT * FROM changed",
            "WITH changed AS (UPDATE users SET name = 'x' RETURNING id) SELECT * FROM changed",
            "SELECT * FROM users FOR UPDATE",
            "SELECT * INTO audit_users FROM users",
            "SELECT 1; SELECT 2",
            "SELECT 1;",
            "SELECT pg_advisory_lock(42)",
            "SELECT pg_try_advisory_lock(42)",
            "SELECT pg_catalog.pg_advisory_lock(42)",
            "SELECT pg_advisory_lock /* no thank you */ (42)",
        ] {
            assert!(
                validate_query(query, LIMIT).is_err(),
                "should reject: {query}"
            );
        }
    }

    #[test]
    fn rejects_placeholders_and_bounds_input_size() {
        assert!(validate_query("SELECT * FROM users WHERE id = $1", LIMIT).is_err());
        assert!(validate_query("SELECT * FROM users WHERE id = ?", LIMIT).is_err());
        assert!(validate_query(&"x".repeat(LIMIT + 1), LIMIT).is_err());
        assert!(validate_query("", LIMIT).is_err());
    }

    #[test]
    fn separates_oversized_row_envelopes_from_data_rows() {
        assert!(matches!(
            super::decode_database_envelope(&serde_json::json!({
                "_adjutant_oversized": true,
                "_adjutant_row_bytes": 65537,
            }))
            .unwrap(),
            super::DatabaseEnvelope::Oversized
        ));
        assert!(matches!(
            super::decode_database_envelope(&serde_json::json!({
                "_adjutant_row": {"id": 1},
            }))
            .unwrap(),
            super::DatabaseEnvelope::Row(_)
        ));
    }
}
