use std::sync::Arc;

use adjutant_mcp::{Config, McpServer, database::Database};
use anyhow::Context;
use axum::{
    Router,
    extract::Request,
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,adjutant_mcp=info")),
        )
        .init();

    let config = Config::from_env()?;
    let max_http_request_bytes = config.max_http_request_bytes;
    let cancellation = CancellationToken::new();
    let database = Arc::new(Database::connect(&config).await?);
    let mcp_database = database.clone();
    let server_config = config.clone();
    let mcp_service: StreamableHttpService<McpServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(McpServer::new(mcp_database.clone(), server_config.clone())),
            Arc::default(),
            StreamableHttpServerConfig::default()
                .with_allowed_hosts(config.allowed_hosts())
                .with_json_response(true)
                .with_cancellation_token(cancellation.child_token()),
        );

    let health_database = database.clone();
    let router = Router::new()
        .route(
            "/healthz",
            get(move || {
                let database = health_database.clone();
                async move {
                    if database.healthcheck().await.is_ok() {
                        (StatusCode::OK, "ok\n")
                    } else {
                        (StatusCode::SERVICE_UNAVAILABLE, "database unavailable\n")
                    }
                }
            }),
        )
        .nest_service("/mcp", mcp_service)
        .layer(middleware::from_fn(log_request))
        // This applies before rmcp sees a request body. Do not depend on MCP
        // parameter extraction to bound a potentially chunked HTTP body.
        .layer(RequestBodyLimitLayer::new(max_http_request_bytes));
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind MCP server to {}", config.bind))?;
    info!(bind = %config.bind, "starting read-only diagnostic MCP server");

    axum::serve(listener, router)
        .with_graceful_shutdown(wait_for_shutdown(cancellation))
        .await
        .context("MCP HTTP server failed")
}

async fn wait_for_shutdown(cancellation: CancellationToken) {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = sigterm.recv() => {},
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not install SIGTERM handler; waiting for Ctrl-C");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    cancellation.cancel();
}

/// Tailscale Serve injects these values after authenticating the tailnet
/// connection. They are audit context only: this process makes no
/// authorization decision based on a caller-controlled HTTP header.
async fn log_request(request: Request, next: Next) -> Response {
    let tailscale_user_login = request
        .headers()
        .get("Tailscale-User-Login")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let tailscale_user_name = request
        .headers()
        .get("Tailscale-User-Name")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    info!(
        %method,
        %path,
        status = response.status().as_u16(),
        tailscale_user_login,
        tailscale_user_name,
        "handled MCP HTTP request"
    );
    response
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::post,
    };
    use tower::ServiceExt;
    use tower_http::limit::RequestBodyLimitLayer;

    #[tokio::test]
    async fn request_body_limit_rejects_oversized_bodies_before_handler_processing() {
        let app = Router::new()
            .route("/mcp", post(|body: String| async move { body }))
            .layer(RequestBodyLimitLayer::new(64));
        let response = app
            .oneshot(
                Request::post("/mcp")
                    .body(Body::from(vec![b'x'; 65]))
                    .expect("test request should be valid"),
            )
            .await
            .expect("router should respond");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
