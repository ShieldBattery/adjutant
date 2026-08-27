use std::sync::Arc;

use rmcp::{
    Json, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    Config,
    database::{
        Database, GameDiagnosticsResult, QueryResult, SchemaResult, UserDiagnosticsResult,
        UserSearchResult,
    },
};

/// Read-only MCP tool provider. It holds a database pool but never exposes its
/// connection string or performs a write-capable query.
#[derive(Clone)]
pub struct McpServer {
    database: Arc<Database>,
    config: Config,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SchemaRequest {
    /// Optional exact PostgreSQL schema, for example `public`.
    schema: Option<String>,
    /// Optional exact table or view name.
    table: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct QueryRequest {
    /// One PostgreSQL read-only query. SELECT, WITH, VALUES, and TABLE are accepted.
    sql: String,
    /// Maximum rows to return. The server applies its configured upper bound.
    max_rows: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchUsersRequest {
    /// Complete or initial characters of a `ShieldBattery` display name. Matching is case-insensitive.
    query: String,
    /// Maximum matches to return. Defaults to 10 and cannot exceed 20.
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct UserDiagnosticsRequest {
    /// Exact `ShieldBattery` user ID. Supply this or `username`, but not both.
    user_id: Option<i32>,
    /// Exact case-insensitive `ShieldBattery` display name. Supply this or `user_id`, but not both.
    username: Option<String>,
    /// Number of recent games to include. Defaults to 10 and cannot exceed 20.
    recent_games_limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GameDiagnosticsRequest {
    /// Exact `ShieldBattery` game UUID.
    game_id: String,
}

impl McpServer {
    #[must_use]
    pub fn new(database: Arc<Database>, config: Config) -> Self {
        Self {
            database,
            config,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl McpServer {
    #[tool(
        description = "Find ShieldBattery users by case-insensitive exact or display-name prefix match. Returns only user ID, display name, and account creation time. Prefer this over raw SQL when resolving a reported username.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn search_users(
        &self,
        Parameters(request): Parameters<SearchUsersRequest>,
    ) -> Result<Json<UserSearchResult>, String> {
        self.database
            .search_users(&request.query, request.limit, &self.config)
            .await
            .map(Json)
            .map_err(|error| format!("user search failed: {error:#}"))
    }

    #[tool(
        description = "Get a bounded diagnostic summary for one ShieldBattery user resolved by ID or exact display name. Returns only non-sensitive identity, aggregate game statistics, current matchmaking ratings, and recent games. Prefer this over raw SQL for user investigations.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn get_user_diagnostics(
        &self,
        Parameters(request): Parameters<UserDiagnosticsRequest>,
    ) -> Result<Json<UserDiagnosticsResult>, String> {
        self.database
            .get_user_diagnostics(
                request.user_id,
                request.username.as_deref(),
                request.recent_games_limit,
                &self.config,
            )
            .await
            .map(Json)
            .map_err(|error| format!("user diagnostics failed: {error:#}"))
    }

    #[tool(
        description = "Get the composed diagnostic record for one ShieldBattery game: core game and map data, participants and result reports, netcode-v2 placement and relay history, desync events, matchmaker formation inputs, and rating changes. Prefer this over raw SQL for game or network incident investigations.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn get_game_diagnostics(
        &self,
        Parameters(request): Parameters<GameDiagnosticsRequest>,
    ) -> Result<Json<GameDiagnosticsResult>, String> {
        let game_id = Uuid::parse_str(&request.game_id)
            .map_err(|error| format!("game_id must be a UUID: {error}"))?;
        self.database
            .get_game_diagnostics(game_id, &self.config)
            .await
            .map(Json)
            .map_err(|error| format!("game diagnostics failed: {error:#}"))
    }

    #[tool(
        description = "List tables, views, and columns accessible to the dedicated read-only database role. Use this before querying unfamiliar data.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn database_schema(
        &self,
        Parameters(request): Parameters<SchemaRequest>,
    ) -> Result<Json<SchemaResult>, String> {
        self.database
            .schema(
                request.schema.as_deref(),
                request.table.as_deref(),
                &self.config,
            )
            .await
            .map(Json)
            .map_err(|error| format!("schema discovery failed: {error:#}"))
    }

    #[tool(
        description = "Run one bounded, read-only PostgreSQL diagnostic query. Results are JSON rows, capped by server-side row, byte, and timeout limits. Writes, locks, multiple statements, and SQL placeholders are rejected.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn query_database(
        &self,
        Parameters(request): Parameters<QueryRequest>,
    ) -> Result<Json<QueryResult>, String> {
        self.database
            .query(&request.sql, request.max_rows, &self.config)
            .await
            .map(Json)
            .map_err(|error| format!("diagnostic query failed: {error:#}"))
    }
}

#[allow(clippy::unused_async_trait_impl)]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                    .with_title("Adjutant ShieldBattery diagnostics")
                    .with_description("Bounded read-only access to curated ShieldBattery database views"),
            )
            .with_instructions(
                "This server is for ShieldBattery production diagnostics. Its database role and all tools are read-only. Prefer search_users, get_user_diagnostics, and get_game_diagnostics for their matching investigations. Use database_schema and query_database only for questions those stable tools do not answer. Request only the data needed and treat all returned user data as confidential.",
            )
    }
}

#[cfg(test)]
mod tests {
    use super::McpServer;

    #[test]
    fn tool_definitions_advertise_read_only_annotations() {
        let search_users = McpServer::search_users_tool_attr();
        let user_diagnostics = McpServer::get_user_diagnostics_tool_attr();
        let game_diagnostics = McpServer::get_game_diagnostics_tool_attr();
        let schema = McpServer::database_schema_tool_attr();
        let query = McpServer::query_database_tool_attr();
        for tool in [
            search_users,
            user_diagnostics,
            game_diagnostics,
            schema,
            query,
        ] {
            let annotations = tool.annotations.expect("annotations should be present");
            assert_eq!(annotations.read_only_hint, Some(true));
            assert_eq!(annotations.idempotent_hint, Some(true));
            assert_eq!(annotations.open_world_hint, Some(false));
        }
    }

    #[test]
    fn tool_router_has_all_database_tools() {
        let router = McpServer::tool_router();
        let names: Vec<_> = router
            .list_all()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(names.len(), 5);
        for expected in [
            "search_users",
            "get_user_diagnostics",
            "get_game_diagnostics",
            "database_schema",
            "query_database",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing {expected}"
            );
        }
    }
}
