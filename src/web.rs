//! Read-only, authenticated inspection UI for Adjutant runs.

use std::fmt::Write as _;
use std::future::Future;
use std::net::SocketAddr;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, REFERRER_POLICY,
    WWW_AUTHENTICATE, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::store::{RunEvent, RunRecord, Store};

const RUN_LIST_LIMIT: i64 = 100;

#[derive(Clone)]
struct AppState {
    store: Store,
    credential_hash: [u8; 32],
}

/// Serve the authenticated, read-only Adjutant inspection UI until `shutdown` resolves.
///
/// The caller should bind `bind` to a Tailscale address (or another private interface) when the
/// UI is intended to be reachable from the tailnet.
pub async fn serve(
    store: Store,
    bind: SocketAddr,
    ui_token: String,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let expected = format!("adjutant:{ui_token}");
    let state = AppState {
        store,
        credential_hash: sha256(expected.as_bytes()),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/runs/{id}", get(run_detail))
        .route("/runs/{id}/events.jsonl", get(events_jsonl))
        .route("/healthz", get(healthz))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

async fn healthz() -> Response {
    secure_response((StatusCode::OK, "ok").into_response())
}

async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authorized(&headers, &state.credential_hash) {
        return unauthorized();
    }

    match state.store.list_runs(RUN_LIST_LIMIT).await {
        Ok(runs) => secure_html(index_page(&runs)),
        Err(_) => internal_error(),
    }
}

async fn run_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !is_authorized(&headers, &state.credential_hash) {
        return unauthorized();
    }

    let run = match state.store.get_run(&id).await {
        Ok(Some(run)) => run,
        Ok(None) => return not_found(),
        Err(_) => return internal_error(),
    };
    match state.store.get_events(&id).await {
        Ok(events) => secure_html(run_page(&run, &events)),
        Err(_) => internal_error(),
    }
}

async fn events_jsonl(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !is_authorized(&headers, &state.credential_hash) {
        return unauthorized();
    }

    match state.store.get_run(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return not_found(),
        Err(_) => return internal_error(),
    }
    match state.store.get_events(&id).await {
        Ok(events) => {
            let mut jsonl = events
                .into_iter()
                .map(|event| event.event_json)
                .collect::<Vec<_>>()
                .join("\n");
            if !jsonl.is_empty() {
                jsonl.push('\n');
            }
            let mut response = jsonl.into_response();
            response.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
            );
            response.headers_mut().insert(
                CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment; filename=adjutant-events.jsonl"),
            );
            secure_response(response)
        }
        Err(_) => internal_error(),
    }
}

fn is_authorized(headers: &HeaderMap, expected_hash: &[u8; 32]) -> bool {
    let credentials = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic_credentials)
        .unwrap_or_default();
    sha256(&credentials).ct_eq(expected_hash).into()
}

fn parse_basic_credentials(value: &str) -> Option<Vec<u8>> {
    let mut parts = value.split_whitespace();
    let scheme = parts.next()?;
    let encoded = parts.next()?;
    if !scheme.eq_ignore_ascii_case("basic") || parts.next().is_some() {
        return None;
    }
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn unauthorized() -> Response {
    let mut response = secure_response((StatusCode::UNAUTHORIZED, "Unauthorized").into_response());
    response.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Adjutant\", charset=\"UTF-8\""),
    );
    response
}

fn not_found() -> Response {
    secure_html(page(
        "Not found",
        "<h1>Not found</h1><p>The requested run does not exist.</p>",
        false,
    ))
    .with_status(StatusCode::NOT_FOUND)
}

fn internal_error() -> Response {
    secure_html(page(
        "Unavailable",
        "<h1>Unavailable</h1><p>The inspection data could not be read.</p>",
        false,
    ))
    .with_status(StatusCode::INTERNAL_SERVER_ERROR)
}

trait ResponseStatus {
    fn with_status(self, status: StatusCode) -> Self;
}

impl ResponseStatus for Response {
    fn with_status(mut self, status: StatusCode) -> Self {
        *self.status_mut() = status;
        self
    }
}

fn secure_html(body: String) -> Response {
    secure_response(Html(body).into_response())
}

fn secure_response(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    );
    response
}

fn index_page(runs: &[RunRecord]) -> String {
    let mut rows = String::new();
    for run in runs {
        let run_path = path_component(&run.id);
        let _ = write!(
            rows,
            "<tr><td><a href=\"/runs/{run_path}\"><code>{}</code></a></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape_html(&run.id),
            escape_html(&run.status),
            escape_html(&run.kind),
            escape_html(&run.title),
            escape_html(&format_timestamp(run.created_at_ms)),
        );
    }
    if rows.is_empty() {
        rows.push_str("<tr><td colspan=\"5\">No runs have been recorded.</td></tr>");
    }
    page(
        "Adjutant runs",
        &format!(
            "<h1>Adjutant runs</h1><p>Latest {RUN_LIST_LIMIT} diagnostic-agent runs.</p><table><thead><tr><th>Run</th><th>Status</th><th>Kind</th><th>Title</th><th>Created</th></tr></thead><tbody>{rows}</tbody></table>"
        ),
        false,
    )
}

fn run_page(run: &RunRecord, events: &[RunEvent]) -> String {
    let mut event_rows = String::new();
    for event in events {
        let _ = write!(
            event_rows,
            "<section class=\"event\"><h3>#{}, {} <small>{}</small></h3><pre>{}</pre></section>",
            event.seq,
            escape_html(&event.kind),
            escape_html(&format_timestamp(event.occurred_at_ms)),
            escape_html(&pretty_json(&event.event_json)),
        );
    }
    if event_rows.is_empty() {
        event_rows.push_str("<p>No recorded agent events yet.</p>");
    }

    let source = discord_message_url(run).map_or_else(
        || "Unavailable".to_owned(),
        |url| format!("<a href=\"{url}\" rel=\"noreferrer\">Open source message in Discord</a>"),
    );
    let refresh = matches!(run.status.as_str(), "queued" | "running");
    let run_path = path_component(&run.id);
    let body = format!(
        "<p><a href=\"/\">&larr; All runs</a></p><h1>{}</h1><p><span class=\"status\">{}</span> &middot; {}</p><dl><dt>Run ID</dt><dd><code>{}</code></dd><dt>Created</dt><dd>{}</dd><dt>Started</dt><dd>{}</dd><dt>Finished</dt><dd>{}</dd><dt>Discord source</dt><dd>{source}</dd></dl><h2>Request</h2><pre>{}</pre>{}<h2>Agent timeline</h2><p><a href=\"/runs/{run_path}/events.jsonl\">Download raw JSONL</a></p>{event_rows}",
        escape_html(&run.title),
        escape_html(&run.status),
        escape_html(&run.kind),
        escape_html(&run.id),
        escape_html(&format_timestamp(run.created_at_ms)),
        escape_option_timestamp(run.started_at_ms),
        escape_option_timestamp(run.finished_at_ms),
        escape_html(&run.request),
        report_sections(run),
    );
    page(&run.title, &body, refresh)
}

fn report_sections(run: &RunRecord) -> String {
    let mut sections = String::new();
    if let Some(manifest) = &run.evidence_manifest {
        let _ = write!(
            sections,
            "<h2>Evidence manifest</h2><pre>{}</pre>",
            escape_html(&pretty_json(manifest))
        );
    }
    if let Some(report) = &run.final_report {
        let _ = write!(
            sections,
            "<h2>Final report</h2><pre>{}</pre>",
            escape_html(report)
        );
    }
    if let Some(error) = &run.error {
        let _ = write!(sections, "<h2>Error</h2><pre>{}</pre>", escape_html(error));
    }
    sections
}

fn discord_message_url(run: &RunRecord) -> Option<String> {
    let ids = [
        &run.discord_guild_id,
        &run.discord_channel_id,
        &run.discord_message_id,
    ];
    ids.iter()
        .all(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| {
            format!(
                "https://discord.com/channels/{}/{}/{}",
                ids[0], ids[1], ids[2]
            )
        })
}

fn page(title: &str, body: &str, refresh: bool) -> String {
    let refresh = if refresh {
        "<meta http-equiv=\"refresh\" content=\"10\">"
    } else {
        ""
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">{refresh}<title>{}</title><style>body{{max-width:72rem;margin:2rem auto;padding:0 1rem;background:#111;color:#eee;font:16px system-ui,sans-serif}}a{{color:#8bc6ff}}table{{border-collapse:collapse;width:100%}}th,td{{padding:.55rem;border-bottom:1px solid #444;text-align:left;vertical-align:top}}pre{{white-space:pre-wrap;overflow-wrap:anywhere;background:#1d1d1d;padding:1rem;border-radius:.25rem}}dl{{display:grid;grid-template-columns:max-content 1fr;gap:.5rem 1rem}}dt{{font-weight:bold}}dd{{margin:0}}.status{{font-weight:bold}}.event{{border-top:1px solid #444;padding-top:.5rem}}small{{font-weight:normal;color:#bbb}}</style></head><body>{body}</body></html>",
        escape_html(title),
    )
}

fn pretty_json(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| raw.to_owned())
}

fn format_timestamp(timestamp_ms: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp_ms) * 1_000_000)
        .ok()
        .and_then(|timestamp| timestamp.format(&Rfc3339).ok())
        .unwrap_or_else(|| format!("{timestamp_ms} ms since Unix epoch"))
}

fn escape_option_timestamp(timestamp_ms: Option<i64>) -> String {
    timestamp_ms.map_or_else(|| "&mdash;".to_owned(), format_timestamp)
}

fn path_component(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                vec![char::from(byte)].into_iter()
            }
            _ => format!("%{byte:02X}")
                .chars()
                .collect::<Vec<_>>()
                .into_iter(),
        })
        .collect()
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html_text_and_attributes() {
        assert_eq!(
            escape_html("<script>& \"' >"),
            "&lt;script&gt;&amp; &quot;&#x27; &gt;"
        );
    }

    #[test]
    fn parses_only_valid_basic_credentials() {
        assert_eq!(
            parse_basic_credentials("Basic YWRqdXRhbnQ6c2VjcmV0"),
            Some(b"adjutant:secret".to_vec())
        );
        assert!(parse_basic_credentials("Bearer YWRqdXRhbnQ6c2VjcmV0").is_none());
        assert!(parse_basic_credentials("Basic not base64").is_none());
        assert!(parse_basic_credentials("Basic YQ== extra").is_none());
    }

    #[test]
    fn percent_encodes_non_path_bytes() {
        assert_eq!(path_component("a/b ?"), "a%2Fb%20%3F");
    }
}
