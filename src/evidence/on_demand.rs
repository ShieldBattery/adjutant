//! Run-scoped evidence collection requested by the diagnostic model.
//!
//! Only typed IDs reach the existing internal API client. The daemon owns downloads and
//! publication; the model's commands retain their read-only, network-disabled sandbox.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tempfile::TempDir;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use super::{CollectionBudget, EvidenceCollector, ManifestEntry};

const MAX_EVIDENCE_REQUESTS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidenceKind {
    BugReport,
    GameArtifacts,
}

impl EvidenceKind {
    const fn directory_prefix(self) -> &'static str {
        match self {
            Self::BugReport => "report",
            Self::GameArtifacts => "game",
        }
    }
}

/// A small receipt, not the report contents. The model reads the files through its sandbox.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct EvidenceReceipt {
    pub(crate) kind: EvidenceKind,
    pub(crate) id: Uuid,
    pub(crate) evidence_dir: std::path::PathBuf,
    pub(crate) manifest_path: std::path::PathBuf,
    pub(crate) file_count: usize,
    pub(crate) cached: bool,
}

#[derive(Clone)]
pub(crate) struct EvidenceRequests(Arc<Inner>);

struct Inner {
    collector: EvidenceCollector,
    directory: Arc<TempDir>,
    state: Mutex<State>,
    published_manifest: RwLock<Vec<ManifestEntry>>,
}

struct State {
    manifest: Vec<ManifestEntry>,
    budget: CollectionBudget,
    collected: HashMap<(EvidenceKind, Uuid), EvidenceReceipt>,
    attempts: usize,
    publication_failed: bool,
}

impl EvidenceRequests {
    pub(super) fn new(
        collector: EvidenceCollector,
        directory: Arc<TempDir>,
        manifest: Vec<ManifestEntry>,
        budget: CollectionBudget,
        initial: Vec<EvidenceReceipt>,
    ) -> Self {
        Self(Arc::new(Inner {
            collector,
            directory,
            published_manifest: RwLock::new(manifest.clone()),
            state: Mutex::new(State {
                manifest,
                budget,
                collected: initial
                    .into_iter()
                    .map(|receipt| ((receipt.kind, receipt.id), receipt))
                    .collect(),
                attempts: 0,
                publication_failed: false,
            }),
        }))
    }

    pub(crate) async fn manifest_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(
            &*self.0.published_manifest.read().await,
        )?)
    }

    /// Serializing collection keeps budget debits, cache hits, and manifest publication ordered.
    /// Dropping the caller cancels network I/O. ZIP workers hold their own staging guard until
    /// bounded extraction finishes, including when the overall investigation is cancelled.
    pub(crate) async fn request(&self, kind: EvidenceKind, id: Uuid) -> Result<EvidenceReceipt> {
        let mut state = self.0.state.lock().await;
        if let Some(cached) = state.collected.get(&(kind, id)) {
            let mut receipt = cached.clone();
            receipt.cached = true;
            return Ok(receipt);
        }
        if state.publication_failed {
            bail!("evidence publication failed earlier; start a new investigation to collect more");
        }
        if state.attempts >= MAX_EVIDENCE_REQUESTS {
            bail!(
                "this investigation has reached its {MAX_EVIDENCE_REQUESTS} evidence request limit"
            );
        }
        state.attempts += 1;
        if self.0.collector.shieldbattery.is_none() {
            bail!("ShieldBattery evidence collection requires SHIELDBATTERY_INTERNAL_URL");
        }

        // Use the same filesystem as the original workspace so directory publication is atomic.
        // The staging directory is a sibling: its own guard also survives blocking ZIP work if
        // the original workspace is dropped during cancellation.
        let staging = Arc::new(
            tempfile::Builder::new()
                .prefix("adjutant-evidence-")
                .tempdir_in(
                    self.0
                        .directory
                        .path()
                        .parent()
                        .context("workspace has no parent")?,
                )?,
        );
        let staged_evidence = staging.path().join("evidence");
        tokio::fs::create_dir(&staged_evidence).await?;
        state.budget.archive.directory_guard = Some(Arc::clone(&staging));
        let collected = match kind {
            EvidenceKind::BugReport => self
                .0
                .collector
                .collect_bug_report(
                    id,
                    staging.path(),
                    &staged_evidence,
                    &mut state.budget.archive,
                )
                .await
                .map(|(_, entries)| entries),
            EvidenceKind::GameArtifacts => {
                self.0
                    .collector
                    .collect_game_artifacts(id, &staged_evidence, &mut state.budget.games)
                    .await
            }
        };
        state.budget.archive.directory_guard = None;
        let entries = collected?;
        let file_count = entries.len();
        tokio::fs::write(
            staged_evidence.join("manifest.json"),
            serde_json::to_string_pretty(&entries)?,
        )
        .await?;

        let relative_directory = format!("requested/{}-{id}", kind.directory_prefix());
        let evidence_root = self.0.directory.path().join("evidence");
        let destination = evidence_root.join(&relative_directory);
        let mut manifest = state.manifest.clone();
        manifest.extend(entries.into_iter().map(|mut entry| {
            entry.path = format!("{relative_directory}/{}", entry.path);
            entry
        }));
        let manifest_json = serde_json::to_string_pretty(&manifest)?;
        tokio::fs::create_dir_all(evidence_root.join("requested")).await?;

        // If publication fails halfway, keep earlier evidence readable but do not retry into a
        // possibly existing directory or pretend that unlisted files were collected successfully.
        state.publication_failed = true;
        let pending_manifest = evidence_root.join(".manifest-next.json");
        tokio::fs::write(&pending_manifest, manifest_json).await?;
        tokio::fs::rename(&staged_evidence, &destination).await?;
        tokio::fs::rename(pending_manifest, evidence_root.join("manifest.json")).await?;
        let receipt = EvidenceReceipt {
            kind,
            id,
            manifest_path: destination.join("manifest.json"),
            evidence_dir: destination,
            file_count,
            cached: false,
        };
        *self.0.published_manifest.write().await = manifest.clone();
        state.manifest = manifest;
        state.collected.insert((kind, id), receipt.clone());
        state.publication_failed = false;
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write as _};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::{Router, body::Body, extract::Path, http::StatusCode, routing::get};
    use serde_json::{Value, json};
    use tokio::sync::Notify;
    use url::Url;
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::evidence::{EvidenceRequest, EvidenceWorkspace, tests::test_config};

    struct Server {
        url: Url,
        requests: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn report_server(gate: Option<(Arc<Notify>, Arc<Notify>)>) -> Server {
        let requests = Arc::new(AtomicUsize::new(0));
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file("client.log", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"synthetic client evidence").unwrap();
        let archive = archive.finish().unwrap().into_inner();
        let app = Router::new()
            .route(
                "/internal/bug-reports/{id}",
                get({
                    let requests = Arc::clone(&requests);
                    move |Path(id): Path<Uuid>| {
                        let requests = Arc::clone(&requests);
                        let gate = gate.clone();
                        async move {
                            requests.fetch_add(1, Ordering::SeqCst);
                            if let Some((entered, release)) = gate {
                                entered.notify_one();
                                release.notified().await;
                            }
                            response(
                                serde_json::to_vec(&json!({"report": {
                                    "id":id, "submitterId":1, "details":"synthetic report",
                                    "logsDeleted":false, "createdAt":1,
                                    "resolvedAt":null, "resolverId":null,
                                }}))
                                .unwrap(),
                                "application/json",
                            )
                        }
                    }
                }),
            )
            .route(
                "/internal/bug-reports/{id}/logs",
                get({
                    let requests = Arc::clone(&requests);
                    move || {
                        let requests = Arc::clone(&requests);
                        let archive = archive.clone();
                        async move {
                            requests.fetch_add(1, Ordering::SeqCst);
                            response(archive, "application/zip")
                        }
                    }
                }),
            );
        serve(app, requests).await
    }

    async fn serve(app: Router, requests: Arc<AtomicUsize>) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Server {
            url,
            requests,
            task,
        }
    }

    fn response(bytes: Vec<u8>, content_type: &'static str) -> axum::http::Response<Body> {
        axum::http::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, content_type)
            .body(Body::from(bytes))
            .unwrap()
    }

    fn request(report: Option<Uuid>) -> EvidenceRequest {
        EvidenceRequest {
            author: "staff".to_owned(),
            text: "investigate".to_owned(),
            bug_report_id: report,
            game_id: None,
            attachments: Vec::new(),
        }
    }

    async fn empty_workspace(url: Url) -> EvidenceWorkspace {
        EvidenceCollector::new(test_config(url))
            .unwrap()
            .collect(Uuid::now_v7(), &request(None))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn publishes_requested_report_and_zip_once_with_updated_manifests() {
        let server = report_server(None).await;
        let workspace = empty_workspace(server.url.clone()).await;
        let id = Uuid::now_v7();
        let (first, second) = tokio::join!(
            workspace.requests.request(EvidenceKind::BugReport, id),
            workspace.requests.request(EvidenceKind::BugReport, id),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_ne!(first.cached, second.cached);
        assert_eq!(first.evidence_dir, second.evidence_dir);
        assert_eq!(server.requests.load(Ordering::SeqCst), 2);
        assert_eq!(first.file_count, 2);
        assert_eq!(
            std::fs::read(first.evidence_dir.join("bug-report-logs/client.log")).unwrap(),
            b"synthetic client evidence"
        );
        let metadata: Value = serde_json::from_slice(
            &std::fs::read(first.evidence_dir.join("bug-report.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["id"], id.to_string());
        let local_manifest: Value =
            serde_json::from_slice(&std::fs::read(first.manifest_path).unwrap()).unwrap();
        assert_eq!(local_manifest.as_array().unwrap().len(), 2);
        let manifest = workspace.requests.manifest_json().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.evidence_dir.join("manifest.json")).unwrap(),
            manifest
        );
        let manifest: Value = serde_json::from_str(&manifest).unwrap();
        for entry in manifest.as_array().unwrap() {
            assert!(
                workspace
                    .evidence_dir
                    .join(entry["path"].as_str().unwrap())
                    .is_file()
            );
        }
    }

    #[tokio::test]
    async fn initial_report_is_cached_and_later_reports_share_archive_limits() {
        let server = report_server(None).await;
        let mut config = (*test_config(server.url.clone())).clone();
        config.max_archive_files = 1;
        let id = Uuid::now_v7();
        let workspace = EvidenceCollector::new(Arc::new(config))
            .unwrap()
            .collect(Uuid::now_v7(), &request(Some(id)))
            .await
            .unwrap();
        let receipt = workspace
            .requests
            .request(EvidenceKind::BugReport, id)
            .await
            .unwrap();
        assert!(receipt.cached);
        assert_eq!(server.requests.load(Ordering::SeqCst), 2);
        let manifest = workspace.requests.manifest_json().await.unwrap();
        let error = workspace
            .requests
            .request(EvidenceKind::BugReport, Uuid::now_v7())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("entry limit"), "{error:#}");
        assert_eq!(workspace.requests.manifest_json().await.unwrap(), manifest);
        assert!(!workspace.evidence_dir.join("requested").exists());
    }

    #[tokio::test]
    async fn requests_game_maps_and_shares_artifact_limits_with_initial_evidence() {
        let requests = Arc::new(AtomicUsize::new(0));
        let map_bytes = b"synthetic map";
        let map_hash = {
            use sha2::Digest as _;
            let mut hash = sha2::Sha256::new();
            hash.update(b"scx");
            hash.update(map_bytes);
            crate::evidence::finish_sha256_hex(hash)
        };
        let map_id = Uuid::now_v7();
        let app = Router::new()
            .route("/internal/games/{id}/artifacts", get({
                let requests = Arc::clone(&requests);
                let map_hash = map_hash.clone();
                move |Path(id): Path<Uuid>| {
                    let requests = Arc::clone(&requests);
                    let map_hash = map_hash.clone();
                    async move {
                        requests.fetch_add(1, Ordering::SeqCst);
                        response(serde_json::to_vec(&json!({
                            "gameId":id, "flightRecordings":[], "replays":[],
                            "map":{"id":map_id, "hash":map_hash, "format":"scx",
                                "name":"test map", "downloadPath":format!("/internal/games/{id}/artifacts/map")},
                        })).unwrap(), "application/json")
                    }
                }
            }))
            .route("/internal/games/{id}/artifacts/map", get({
                let requests = Arc::clone(&requests);
                move || {
                    requests.fetch_add(1, Ordering::SeqCst);
                    async move { response(map_bytes.to_vec(), "application/octet-stream") }
                }
            }));
        let server = serve(app, requests).await;
        let mut config = (*test_config(server.url.clone())).clone();
        config.max_game_artifacts = 2;
        let initial_game = Uuid::now_v7();
        let mut initial_request = request(None);
        initial_request.game_id = Some(initial_game);
        let workspace = EvidenceCollector::new(Arc::new(config))
            .unwrap()
            .collect(Uuid::now_v7(), &initial_request)
            .await
            .unwrap();
        let cached = workspace
            .requests
            .request(EvidenceKind::GameArtifacts, initial_game)
            .await
            .unwrap();
        assert!(cached.cached);
        assert_eq!(server.requests.load(Ordering::SeqCst), 2);
        let later_game = Uuid::now_v7();
        let receipt = workspace
            .requests
            .request(EvidenceKind::GameArtifacts, later_game)
            .await
            .unwrap();
        assert!(!receipt.cached);
        assert_eq!(receipt.file_count, 2);
        assert_eq!(
            std::fs::read(
                receipt
                    .evidence_dir
                    .join(format!("game-{later_game}/map/{map_hash}.scx"))
            )
            .unwrap(),
            map_bytes
        );
        let manifest = workspace.requests.manifest_json().await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&manifest)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert!(
            workspace
                .requests
                .request(EvidenceKind::GameArtifacts, Uuid::now_v7())
                .await
                .unwrap_err()
                .to_string()
                .contains("artifact limit")
        );
        assert_eq!(workspace.requests.manifest_json().await.unwrap(), manifest);
        assert_eq!(server.requests.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn manifest_snapshot_stays_responsive_while_a_request_is_downloading() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let server = report_server(Some((Arc::clone(&entered), Arc::clone(&release)))).await;
        let workspace = empty_workspace(server.url.clone()).await;
        let requests = workspace.requests.clone();
        let pending = tokio::spawn(async move {
            requests
                .request(EvidenceKind::BugReport, Uuid::now_v7())
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(
                Duration::from_millis(100),
                workspace.requests.manifest_json()
            )
            .await
            .unwrap()
            .unwrap(),
            "[]"
        );
        release.notify_one();
        pending.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn failed_requests_consume_the_attempt_limit_without_publishing_files() {
        let requests = Arc::new(AtomicUsize::new(0));
        let app = Router::new().fallback({
            let requests = Arc::clone(&requests);
            move || {
                requests.fetch_add(1, Ordering::SeqCst);
                async { StatusCode::NOT_FOUND }
            }
        });
        let server = serve(app, requests).await;
        let workspace = empty_workspace(server.url.clone()).await;
        for _ in 0..MAX_EVIDENCE_REQUESTS {
            assert!(
                workspace
                    .requests
                    .request(EvidenceKind::BugReport, Uuid::now_v7())
                    .await
                    .is_err()
            );
        }
        let error = workspace
            .requests
            .request(EvidenceKind::BugReport, Uuid::now_v7())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("request limit"));
        assert_eq!(
            server.requests.load(Ordering::SeqCst),
            MAX_EVIDENCE_REQUESTS
        );
        assert_eq!(workspace.requests.manifest_json().await.unwrap(), "[]");
    }
}
