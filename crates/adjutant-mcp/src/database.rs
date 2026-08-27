use std::{
    fmt::Write,
    ops::ControlFlow,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use futures_util::TryStreamExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlparser::{
    ast::{Query, Select, Statement, Visit, Visitor},
    dialect::PostgreSqlDialect,
    parser::Parser,
    tokenizer::{Token, Tokenizer},
};
use sqlx::{
    AssertSqlSafe, PgPool,
    postgres::{PgArguments, PgPoolOptions},
    query::QueryScalar,
    types::Json,
};
use tracing::{info, warn};
use uuid::Uuid;

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

const DEFAULT_USER_SEARCH_LIMIT: u32 = 10;
const MAX_USER_SEARCH_LIMIT: u32 = 20;
const DEFAULT_RECENT_GAMES_LIMIT: u32 = 10;
const MAX_RECENT_GAMES_LIMIT: u32 = 20;

/// A small, non-sensitive account record used to identify a diagnostic subject.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct UserSummary {
    pub id: i32,
    pub name: String,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct UserSearchResult {
    pub matches: Vec<UserSummary>,
    pub truncated: bool,
    #[serde(default)]
    pub response_bytes: usize,
    #[serde(default)]
    pub duration_ms: u128,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CurrentMatchmakingRating {
    pub matchmaking_type: String,
    pub season_id: i32,
    pub season_name: String,
    pub season_start: String,
    pub rating: Option<f64>,
    pub uncertainty: Option<f64>,
    pub volatility: Option<f64>,
    pub points: Option<f64>,
    pub points_converged: Option<bool>,
    pub bonus_used: Option<f64>,
    pub num_games_played: i32,
    pub lifetime_games: Option<i32>,
    pub wins: Option<i32>,
    pub losses: Option<i32>,
    pub last_played_at: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RecentGame {
    pub game_id: String,
    pub start_time: String,
    pub map_id: String,
    pub map_name: Option<String>,
    pub game_length_ms: Option<i32>,
    pub game_source: Option<String>,
    pub matchmaking_type: Option<String>,
    pub selected_race: String,
    pub assigned_race: Option<String>,
    pub team: Option<i32>,
    pub result: Option<String>,
    pub apm: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct UserStats {
    pub p_wins: i32,
    pub p_losses: i32,
    pub t_wins: i32,
    pub t_losses: i32,
    pub z_wins: i32,
    pub z_losses: i32,
    pub r_wins: i32,
    pub r_losses: i32,
    pub r_p_wins: i32,
    pub r_p_losses: i32,
    pub r_t_wins: i32,
    pub r_t_losses: i32,
    pub r_z_wins: i32,
    pub r_z_losses: i32,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct UserDiagnosticsResult {
    pub found: bool,
    pub user: Option<UserSummary>,
    /// The curated view exposes only race win/loss totals. It intentionally
    /// omits profile, network, authentication, and moderation data.
    pub stats: Option<UserStats>,
    pub current_matchmaking_ratings: Vec<CurrentMatchmakingRating>,
    pub current_matchmaking_ratings_truncated: bool,
    pub recent_games: Vec<RecentGame>,
    pub recent_games_truncated: bool,
    #[serde(default)]
    pub response_bytes: usize,
    #[serde(default)]
    pub duration_ms: u128,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GameSummary {
    pub id: String,
    pub start_time: String,
    pub map_id: String,
    pub map_name: Option<String>,
    pub map_hash: Option<String>,
    pub config: Value,
    pub results: Option<Value>,
    pub disputable: bool,
    pub dispute_requested: bool,
    pub dispute_reviewed: bool,
    pub game_length_ms: Option<i32>,
    pub selected_matchup: Option<String>,
    pub assigned_matchup: Option<String>,
    pub manually_resolved_at: Option<String>,
    pub netcode_v2_session: Option<String>,
    pub netcode_v2_relays: Value,
    pub netcode_v2_requested_regions: Value,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GameParticipant {
    pub user_id: i32,
    pub user_name: String,
    pub start_time: String,
    pub selected_race: String,
    pub assigned_race: Option<String>,
    pub team: Option<i32>,
    pub reported_results: Option<Value>,
    pub reported_at: Option<String>,
    pub result: Option<String>,
    pub apm: Option<i32>,
    pub replay_file_id: Option<String>,
    pub departure_kind: Option<String>,
    pub departure_time: Option<String>,
    pub relay_report_time: Option<String>,
    pub relay_report_frame: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GameDesyncEvent {
    pub ordinal: i64,
    pub detected_at: String,
    pub received_at: String,
    pub frame: Option<i32>,
    pub no_majority: bool,
    pub diverged_user_ids: Vec<i32>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MatchmakingFormation {
    pub r#type: String,
    pub quality: Option<f64>,
    pub skill_variance: Option<f64>,
    pub win_probability: Option<f64>,
    pub team_a_rating: Option<f64>,
    pub team_b_rating: Option<f64>,
    pub max_latency: Option<f64>,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MatchmakingRatingChange {
    pub user_id: i32,
    pub user_name: String,
    pub r#type: String,
    pub change_date: String,
    pub outcome: String,
    pub rating: Option<f64>,
    pub rating_change: Option<f64>,
    pub uncertainty: Option<f64>,
    pub uncertainty_change: Option<f64>,
    pub volatility: Option<f64>,
    pub volatility_change: Option<f64>,
    pub points: Option<f64>,
    pub points_change: Option<f64>,
    pub probability: Option<f64>,
    pub lifetime_games: Option<i32>,
}

#[allow(clippy::struct_excessive_bools)] // Truncation state is part of this stable response shape.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GameDiagnosticsResult {
    pub found: bool,
    pub game: Option<GameSummary>,
    pub participants: Vec<GameParticipant>,
    pub participants_truncated: bool,
    pub desync_events: Vec<GameDesyncEvent>,
    pub desync_events_truncated: bool,
    pub matchmaking_formation: Option<MatchmakingFormation>,
    pub rating_changes: Vec<MatchmakingRatingChange>,
    pub rating_changes_truncated: bool,
    #[serde(default)]
    pub response_bytes: usize,
    #[serde(default)]
    pub duration_ms: u128,
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

fn decode_bounded_diagnostic_payload(
    envelope: &Value,
    max_response_bytes: usize,
) -> Result<(Value, usize)> {
    let payload = match decode_database_envelope(envelope)? {
        DatabaseEnvelope::Row(payload) => payload,
        DatabaseEnvelope::Oversized => {
            bail!("diagnostic response exceeded the configured response size limit")
        }
    };
    let response_bytes = serde_json::to_vec(&payload)
        .context("failed to encode bounded diagnostic response")?
        .len();
    if response_bytes > max_response_bytes {
        bail!("diagnostic response exceeded the configured response size limit");
    }
    Ok((payload, response_bytes))
}

fn validate_user_search_query(query: &str) -> Result<()> {
    if query.trim().is_empty() {
        bail!("query must not be empty");
    }
    if query.chars().count() > 32 {
        bail!("query must contain at most 32 characters");
    }
    Ok(())
}

fn validate_exact_username(username: &str) -> Result<()> {
    if username.trim().is_empty() {
        bail!("username must not be empty");
    }
    if username.chars().count() > 32 {
        bail!("username must contain at most 32 characters");
    }
    Ok(())
}

fn escape_like_prefix(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len());
    for character in query.chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn bounded_limit(
    requested_limit: Option<u32>,
    default_limit: u32,
    hard_max: u32,
    config: &Config,
    name: &str,
) -> Result<u32> {
    let limit = requested_limit
        .unwrap_or(default_limit)
        .min(hard_max)
        .min(config.max_rows);
    if limit == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(limit)
}

fn diagnostic_component_limit(config: &Config) -> Result<u32> {
    if config.max_rows == 0 {
        bail!("configured maximum rows must be greater than zero");
    }
    Ok(config.max_rows)
}

trait DiagnosticResponse: DeserializeOwned {
    fn set_metadata(&mut self, response_bytes: usize, duration_ms: u128);
    fn response_bytes(&self) -> usize;
    fn duration_ms(&self) -> u128;
    fn item_count(&self) -> usize;
    fn is_truncated(&self) -> bool;
}

impl DiagnosticResponse for UserSearchResult {
    fn set_metadata(&mut self, response_bytes: usize, duration_ms: u128) {
        self.response_bytes = response_bytes;
        self.duration_ms = duration_ms;
    }

    fn response_bytes(&self) -> usize {
        self.response_bytes
    }

    fn duration_ms(&self) -> u128 {
        self.duration_ms
    }

    fn item_count(&self) -> usize {
        self.matches.len()
    }

    fn is_truncated(&self) -> bool {
        self.truncated
    }
}

impl DiagnosticResponse for UserDiagnosticsResult {
    fn set_metadata(&mut self, response_bytes: usize, duration_ms: u128) {
        self.response_bytes = response_bytes;
        self.duration_ms = duration_ms;
    }

    fn response_bytes(&self) -> usize {
        self.response_bytes
    }

    fn duration_ms(&self) -> u128 {
        self.duration_ms
    }

    fn item_count(&self) -> usize {
        self.recent_games.len()
    }

    fn is_truncated(&self) -> bool {
        self.current_matchmaking_ratings_truncated || self.recent_games_truncated
    }
}

impl DiagnosticResponse for GameDiagnosticsResult {
    fn set_metadata(&mut self, response_bytes: usize, duration_ms: u128) {
        self.response_bytes = response_bytes;
        self.duration_ms = duration_ms;
    }

    fn response_bytes(&self) -> usize {
        self.response_bytes
    }

    fn duration_ms(&self) -> u128 {
        self.duration_ms
    }

    fn item_count(&self) -> usize {
        self.participants.len() + self.desync_events.len() + self.rating_changes.len()
    }

    fn is_truncated(&self) -> bool {
        self.participants_truncated || self.desync_events_truncated || self.rating_changes_truncated
    }
}

// These statements deliberately name only the curated diagnostic views. Their
// parameters are bound by sqlx; no report, account, or game value is ever
// interpolated into SQL text.
const SEARCH_USERS_SQL: &str = r"
    WITH candidates AS (
        SELECT id, name, created, lower(name) = lower($1::text) AS exact_match
        FROM adjutant_diagnostics.users
        WHERE lower(name) = lower($1::text)
           OR name ILIKE $2::text || '%' ESCAPE E'\\'
        ORDER BY lower(name) = lower($1::text) DESC, lower(name), id
        LIMIT $3
    ),
    response AS (
        SELECT jsonb_build_object(
            'matches', COALESCE((
                SELECT jsonb_agg(jsonb_build_object(
                    'id', id,
                    'name', name,
                    'created_at', created::text
                ) ORDER BY exact_match DESC, lower(name), id)
                FROM (
                    SELECT * FROM candidates
                    ORDER BY exact_match DESC, lower(name), id
                    LIMIT ($3 - 1)
                ) AS bounded_matches
            ), '[]'::jsonb),
            'truncated', (SELECT count(*) = $3 FROM candidates)
        ) AS payload
    )
    SELECT CASE
        WHEN octet_length(payload::text) <= $4 THEN
            jsonb_build_object('_adjutant_row', payload)
        ELSE jsonb_build_object(
            '_adjutant_oversized', true,
            '_adjutant_row_bytes', octet_length(payload::text)
        )
    END
    FROM response
";

const USER_DIAGNOSTICS_SQL: &str = r"
    WITH target_user AS (
        SELECT id, name, created
        FROM adjutant_diagnostics.users
        WHERE ($1::integer IS NOT NULL AND id = $1)
           OR ($2::text IS NOT NULL AND lower(name) = lower($2))
        ORDER BY id
        LIMIT 1
    ),
    rating_rows AS (
        SELECT matchmaking_type, season_id, season_name, season_start_date,
               rating, uncertainty, volatility, points, points_converged,
               bonus_used, num_games_played, lifetime_games, wins, losses,
               last_played_date
        FROM adjutant_diagnostics.current_matchmaking_ratings
        WHERE user_id = (SELECT id FROM target_user)
        ORDER BY matchmaking_type, season_id DESC
        LIMIT $4
    ),
    recent_game_rows AS (
        SELECT g.id AS game_id, g.start_time, g.map_id, m.name AS map_name,
               g.game_length, g.config ->> 'gameSource' AS game_source,
               g.config -> 'gameSourceExtra' ->> 'type' AS matchmaking_type,
               gu.selected_race, gu.assigned_race, gu.team, gu.result, gu.apm
        FROM target_user AS target
        JOIN adjutant_diagnostics.game_users AS gu ON gu.user_id = target.id
        JOIN adjutant_diagnostics.games AS g ON g.id = gu.game_id
        LEFT JOIN adjutant_diagnostics.maps AS m ON m.id = g.map_id
        ORDER BY g.start_time DESC, g.id DESC
        LIMIT $3
    ),
    response AS (
        SELECT jsonb_build_object(
            'found', EXISTS(SELECT 1 FROM target_user),
            'user', (
                SELECT jsonb_build_object(
                    'id', id,
                    'name', name,
                    'created_at', created::text
                )
                FROM target_user
            ),
            'stats', (
                SELECT jsonb_build_object(
                    'p_wins', p_wins,
                    'p_losses', p_losses,
                    't_wins', t_wins,
                    't_losses', t_losses,
                    'z_wins', z_wins,
                    'z_losses', z_losses,
                    'r_wins', r_wins,
                    'r_losses', r_losses,
                    'r_p_wins', r_p_wins,
                    'r_p_losses', r_p_losses,
                    'r_t_wins', r_t_wins,
                    'r_t_losses', r_t_losses,
                    'r_z_wins', r_z_wins,
                    'r_z_losses', r_z_losses
                )
                FROM adjutant_diagnostics.user_stats
                WHERE user_id = (SELECT id FROM target_user)
            ),
            'current_matchmaking_ratings', COALESCE((
                SELECT jsonb_agg(jsonb_build_object(
                    'matchmaking_type', matchmaking_type,
                    'season_id', season_id,
                    'season_name', season_name,
                    'season_start', season_start_date::text,
                    'rating', rating,
                    'uncertainty', uncertainty,
                    'volatility', volatility,
                    'points', points,
                    'points_converged', points_converged,
                    'bonus_used', bonus_used,
                    'num_games_played', num_games_played,
                    'lifetime_games', lifetime_games,
                    'wins', wins,
                    'losses', losses,
                    'last_played_at', last_played_date::text
                ) ORDER BY matchmaking_type, season_id DESC)
                FROM (
                    SELECT * FROM rating_rows
                    ORDER BY matchmaking_type, season_id DESC
                    LIMIT ($4 - 1)
                ) AS bounded_ratings
            ), '[]'::jsonb),
            'current_matchmaking_ratings_truncated', (
                SELECT count(*) = $4 FROM rating_rows
            ),
            'recent_games', COALESCE((
                SELECT jsonb_agg(jsonb_build_object(
                    'game_id', game_id::text,
                    'start_time', start_time::text,
                    'map_id', map_id::text,
                    'map_name', map_name,
                    'game_length_ms', game_length,
                    'game_source', game_source,
                    'matchmaking_type', matchmaking_type,
                    'selected_race', selected_race,
                    'assigned_race', assigned_race,
                    'team', team,
                    'result', result,
                    'apm', apm
                ) ORDER BY start_time DESC, game_id DESC)
                FROM (
                    SELECT * FROM recent_game_rows
                    ORDER BY start_time DESC, game_id DESC
                    LIMIT ($3 - 1)
                ) AS bounded_games
            ), '[]'::jsonb),
            'recent_games_truncated', (SELECT count(*) = $3 FROM recent_game_rows)
        ) AS payload
    )
    SELECT CASE
        WHEN octet_length(payload::text) <= $5 THEN
            jsonb_build_object('_adjutant_row', payload)
        ELSE jsonb_build_object(
            '_adjutant_oversized', true,
            '_adjutant_row_bytes', octet_length(payload::text)
        )
    END
    FROM response
";

const GAME_DIAGNOSTICS_SQL: &str = r"
    WITH target_game AS (
        SELECT g.id, g.start_time, g.map_id, m.name AS map_name, m.map_hash,
               g.config, g.results, g.disputable, g.dispute_requested,
               g.dispute_reviewed, g.game_length, g.selected_matchup,
               g.assigned_matchup, g.manually_resolved_at, g.netcode_v2_session,
               g.netcode_v2_relays, g.netcode_v2_requested_regions
        FROM adjutant_diagnostics.games AS g
        LEFT JOIN adjutant_diagnostics.maps AS m ON m.id = g.map_id
        WHERE g.id = $1
        LIMIT 1
    ),
    participant_rows AS (
        SELECT gu.user_id, u.name AS user_name, gu.start_time, gu.selected_race,
               gu.assigned_race, gu.team, gu.reported_results,
               gu.reported_at, gu.result, gu.apm, gu.replay_file_id,
               gu.departure_kind, gu.departure_time, gu.relay_report_time,
               gu.relay_report_frame
        FROM adjutant_diagnostics.game_users AS gu
        JOIN adjutant_diagnostics.users AS u ON u.id = gu.user_id
        WHERE gu.game_id = (SELECT id FROM target_game)
        ORDER BY gu.user_id
        LIMIT $2
    ),
    desync_rows AS (
        SELECT sync_ordinal, detected_at, received_at, game_frame, no_majority,
               diverged_user_ids
        FROM adjutant_diagnostics.game_desync_events
        WHERE game_id = (SELECT id FROM target_game)
        ORDER BY sync_ordinal
        LIMIT $2
    ),
    formation_row AS (
        SELECT matchmaking_type AS type, quality, skill_variance, win_probability, team_a_rating,
               team_b_rating, max_latency, created_at
        FROM adjutant_diagnostics.matchmaking_formations
        WHERE game_id = (SELECT id FROM target_game)
        ORDER BY created_at DESC
        LIMIT 1
    ),
    rating_change_rows AS (
        SELECT changes.user_id, users.name AS user_name, changes.matchmaking_type AS type,
               changes.change_date, changes.outcome, changes.rating,
               changes.rating_change, changes.uncertainty,
               changes.uncertainty_change, changes.volatility,
               changes.volatility_change, changes.points, changes.points_change,
               changes.probability, changes.lifetime_games
        FROM adjutant_diagnostics.matchmaking_rating_changes AS changes
        JOIN adjutant_diagnostics.users AS users ON users.id = changes.user_id
        WHERE changes.game_id = (SELECT id FROM target_game)
        ORDER BY changes.user_id, changes.matchmaking_type
        LIMIT $2
    ),
    response AS (
        SELECT jsonb_build_object(
            'found', EXISTS(SELECT 1 FROM target_game),
            'game', (
                SELECT jsonb_build_object(
                    'id', id::text,
                    'start_time', start_time::text,
                    'map_id', map_id::text,
                    'map_name', map_name,
                    'map_hash', map_hash,
                    'config', config,
                    'results', results,
                    'disputable', disputable,
                    'dispute_requested', dispute_requested,
                    'dispute_reviewed', dispute_reviewed,
                    'game_length_ms', game_length,
                    'selected_matchup', selected_matchup,
                    'assigned_matchup', assigned_matchup,
                    'manually_resolved_at', manually_resolved_at::text,
                    'netcode_v2_session', netcode_v2_session::text,
                    'netcode_v2_relays', COALESCE(netcode_v2_relays, '[]'::jsonb),
                    'netcode_v2_requested_regions', COALESCE(netcode_v2_requested_regions, '[]'::jsonb)
                )
                FROM target_game
            ),
            'participants', COALESCE((
                SELECT jsonb_agg(jsonb_build_object(
                    'user_id', user_id,
                    'user_name', user_name,
                    'start_time', start_time::text,
                    'selected_race', selected_race,
                    'assigned_race', assigned_race,
                    'team', team,
                    'reported_results', reported_results,
                    'reported_at', reported_at::text,
                    'result', result,
                    'apm', apm,
                    'replay_file_id', replay_file_id::text,
                    'departure_kind', departure_kind,
                    'departure_time', departure_time::text,
                    'relay_report_time', relay_report_time::text,
                    'relay_report_frame', relay_report_frame
                ) ORDER BY user_id)
                FROM (
                    SELECT * FROM participant_rows ORDER BY user_id LIMIT ($2 - 1)
                ) AS bounded_participants
            ), '[]'::jsonb),
            'participants_truncated', (SELECT count(*) = $2 FROM participant_rows),
            'desync_events', COALESCE((
                SELECT jsonb_agg(jsonb_build_object(
                    'ordinal', sync_ordinal,
                    'detected_at', detected_at::text,
                    'received_at', received_at::text,
                    'frame', game_frame,
                    'no_majority', no_majority,
                    'diverged_user_ids', diverged_user_ids
                ) ORDER BY sync_ordinal)
                FROM (
                    SELECT * FROM desync_rows ORDER BY sync_ordinal LIMIT ($2 - 1)
                ) AS bounded_desync_events
            ), '[]'::jsonb),
            'desync_events_truncated', (SELECT count(*) = $2 FROM desync_rows),
            'matchmaking_formation', (
                SELECT jsonb_build_object(
                    'type', type,
                    'quality', quality,
                    'skill_variance', skill_variance,
                    'win_probability', win_probability,
                    'team_a_rating', team_a_rating,
                    'team_b_rating', team_b_rating,
                    'max_latency', max_latency,
                    'created_at', created_at::text
                )
                FROM formation_row
            ),
            'rating_changes', COALESCE((
                SELECT jsonb_agg(jsonb_build_object(
                    'user_id', user_id,
                    'user_name', user_name,
                    'type', type,
                    'change_date', change_date::text,
                    'outcome', outcome,
                    'rating', rating,
                    'rating_change', rating_change,
                    'uncertainty', uncertainty,
                    'uncertainty_change', uncertainty_change,
                    'volatility', volatility,
                    'volatility_change', volatility_change,
                    'points', points,
                    'points_change', points_change,
                    'probability', probability,
                    'lifetime_games', lifetime_games
                ) ORDER BY user_id, type)
                FROM (
                    SELECT * FROM rating_change_rows
                    ORDER BY user_id, type
                    LIMIT ($2 - 1)
                ) AS bounded_rating_changes
            ), '[]'::jsonb),
            'rating_changes_truncated', (SELECT count(*) = $2 FROM rating_change_rows)
        ) AS payload
    )
    SELECT CASE
        WHEN octet_length(payload::text) <= $3 THEN
            jsonb_build_object('_adjutant_row', payload)
        ELSE jsonb_build_object(
            '_adjutant_oversized', true,
            '_adjutant_row_bytes', octet_length(payload::text)
        )
    END
    FROM response
";

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

    /// Finds a small, ranked set of accounts without exposing private profile data.
    pub async fn search_users(
        &self,
        query: &str,
        requested_limit: Option<u32>,
        config: &Config,
    ) -> Result<UserSearchResult> {
        let started = Instant::now();
        let result = async {
            validate_user_search_query(query)?;
            let limit = bounded_limit(
                requested_limit,
                DEFAULT_USER_SEARCH_LIMIT,
                MAX_USER_SEARCH_LIMIT,
                config,
                "limit",
            )?;
            let escaped_prefix = escape_like_prefix(query);
            self.execute_bounded_diagnostic(
                sqlx::query_scalar::<_, Json<Value>>(SEARCH_USERS_SQL)
                    .bind(query)
                    .bind(escaped_prefix)
                    .bind(i64::from(limit) + 1)
                    .bind(
                        i64::try_from(config.max_response_bytes)
                            .context("response limit is too large")?,
                    ),
                config,
            )
            .await
        }
        .await;
        log_diagnostic_operation("search_users", started, &result);
        result
    }

    /// Retrieves the curated diagnostic history for one account.
    pub async fn get_user_diagnostics(
        &self,
        user_id: Option<i32>,
        username: Option<&str>,
        recent_games_limit: Option<u32>,
        config: &Config,
    ) -> Result<UserDiagnosticsResult> {
        let started = Instant::now();
        let result = async {
            if user_id.is_some() == username.is_some() {
                bail!("exactly one of user_id or username is required");
            }
            if user_id.is_some_and(|id| id <= 0) {
                bail!("user_id must be greater than zero");
            }
            if let Some(username) = username {
                validate_exact_username(username)?;
            }

            let recent_limit = bounded_limit(
                recent_games_limit,
                DEFAULT_RECENT_GAMES_LIMIT,
                MAX_RECENT_GAMES_LIMIT,
                config,
                "recent_games_limit",
            )?;
            let component_limit = diagnostic_component_limit(config)?;
            let username = username.map(str::to_owned);
            self.execute_bounded_diagnostic(
                sqlx::query_scalar::<_, Json<Value>>(USER_DIAGNOSTICS_SQL)
                    .bind(user_id)
                    .bind(username.as_deref())
                    .bind(i64::from(recent_limit) + 1)
                    .bind(i64::from(component_limit) + 1)
                    .bind(
                        i64::try_from(config.max_response_bytes)
                            .context("response limit is too large")?,
                    ),
                config,
            )
            .await
        }
        .await;
        log_diagnostic_operation("get_user_diagnostics", started, &result);
        result
    }

    /// Retrieves the bounded, cross-cutting diagnostics for one game UUID.
    pub async fn get_game_diagnostics(
        &self,
        game_id: Uuid,
        config: &Config,
    ) -> Result<GameDiagnosticsResult> {
        let started = Instant::now();
        let result = async {
            let component_limit = diagnostic_component_limit(config)?;
            self.execute_bounded_diagnostic(
                sqlx::query_scalar::<_, Json<Value>>(GAME_DIAGNOSTICS_SQL)
                    .bind(game_id)
                    .bind(i64::from(component_limit) + 1)
                    .bind(
                        i64::try_from(config.max_response_bytes)
                            .context("response limit is too large")?,
                    ),
                config,
            )
            .await
        }
        .await;
        log_diagnostic_operation("get_game_diagnostics", started, &result);
        result
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

    async fn execute_bounded_diagnostic<T>(
        &self,
        query: QueryScalar<'_, sqlx::Postgres, Json<Value>, PgArguments>,
        config: &Config,
    ) -> Result<T>
    where
        T: DiagnosticResponse,
    {
        let started = Instant::now();
        let mut transaction = self.read_only_transaction(config).await?;
        let payload = async {
            let Json(envelope) = query
                .fetch_one(&mut *transaction)
                .await
                .context("purpose-built diagnostic query failed")?;
            decode_bounded_diagnostic_payload(&envelope, config.max_response_bytes)
        }
        .await;
        transaction
            .rollback()
            .await
            .context("failed to close purpose-built diagnostic transaction")?;

        let (payload, response_bytes) = payload?;
        let mut response: T = serde_json::from_value(payload)
            .context("database returned an invalid purpose-built diagnostic response")?;
        response.set_metadata(response_bytes, started.elapsed().as_millis());
        Ok(response)
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

fn log_diagnostic_operation<T>(operation: &'static str, started: Instant, result: &Result<T>)
where
    T: DiagnosticResponse,
{
    if let Ok(response) = result {
        info!(
            operation,
            duration_ms = response.duration_ms(),
            count = response.item_count(),
            truncated = response.is_truncated(),
            response_bytes = response.response_bytes(),
            "completed purpose-built database diagnostic"
        );
    } else {
        warn!(
            operation,
            duration_ms = started.elapsed().as_millis(),
            "purpose-built database diagnostic failed"
        );
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
    use super::{
        DiagnosticResponse, UserSearchResult, decode_bounded_diagnostic_payload,
        escape_like_prefix, validate_exact_username, validate_query, validate_user_search_query,
    };

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

    #[test]
    fn user_lookup_input_is_bounded_by_unicode_scalar_values() {
        assert!(validate_user_search_query("Zergling").is_ok());
        assert!(validate_user_search_query("  ").is_err());
        assert!(validate_user_search_query(&"🛡".repeat(32)).is_ok());
        assert!(validate_user_search_query(&"🛡".repeat(33)).is_err());
        assert!(validate_exact_username("Hydralisk").is_ok());
        assert!(validate_exact_username(&"a".repeat(33)).is_err());
    }

    #[test]
    fn search_prefix_escapes_like_metacharacters() {
        assert_eq!(escape_like_prefix("100%_ready\\go"), "100\\%\\_ready\\\\go");
        assert_eq!(escape_like_prefix("plain text"), "plain text");
    }

    #[test]
    fn bounded_payload_envelope_is_deserialized_before_metadata_is_added() {
        let payload = serde_json::json!({
            "matches": [],
            "truncated": false,
        });
        let envelope = serde_json::json!({"_adjutant_row": payload});
        let (payload, response_bytes) =
            decode_bounded_diagnostic_payload(&envelope, 1024).expect("payload should fit");
        assert_eq!(response_bytes, serde_json::to_vec(&payload).unwrap().len());

        let mut response: UserSearchResult = serde_json::from_value(payload).unwrap();
        response.set_metadata(response_bytes, 17);
        assert_eq!(response.response_bytes, response_bytes);
        assert_eq!(response.duration_ms, 17);
    }

    #[test]
    fn bounded_payload_rejects_the_database_oversized_envelope() {
        let envelope = serde_json::json!({
            "_adjutant_oversized": true,
            "_adjutant_row_bytes": 2048,
        });
        let error = decode_bounded_diagnostic_payload(&envelope, 1024).unwrap_err();
        assert!(error.to_string().contains("response size limit"));
    }
}
