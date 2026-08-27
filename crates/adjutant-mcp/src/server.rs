use std::sync::Arc;

use rmcp::{
    Json, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::{
    Config,
    database::{Database, QueryResult, SchemaResult},
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
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "This server is for ShieldBattery production diagnostics. Its database role and query validator are read-only. Discover the accessible schema first, request only the data needed for the investigation, and treat all returned user data as confidential.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::McpServer;

    #[test]
    fn tool_definitions_advertise_read_only_annotations() {
        let schema = McpServer::database_schema_tool_attr();
        let query = McpServer::query_database_tool_attr();
        for tool in [schema, query] {
            let annotations = tool.annotations.expect("annotations should be present");
            assert_eq!(annotations.read_only_hint, Some(true));
            assert_eq!(annotations.idempotent_hint, Some(true));
            assert_eq!(annotations.open_world_hint, Some(false));
        }
    }

    #[test]
    fn tool_router_has_both_database_tools() {
        let router = McpServer::tool_router();
        let names: Vec<_> = router
            .list_all()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert!(names.iter().any(|name| name == "database_schema"));
        assert!(names.iter().any(|name| name == "query_database"));
    }
}
