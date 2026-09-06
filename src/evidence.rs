use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, Response, redirect::Policy};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use url::Url;
use uuid::Uuid;
use zip::ZipArchive;

use crate::config::Config;
use crate::shieldbattery::GameArtifacts;
use crate::shieldbattery::{BugReport, ShieldBatteryClient};

const MAX_ARCHIVE_PATH_DEPTH: usize = 8;

#[derive(Clone, Debug)]
pub struct Attachment {
    pub url: Url,
    pub filename: String,
    pub size: u64,
    pub content_type: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EvidenceRequest {
    pub author: String,
    pub text: String,
    pub bug_report_id: Option<Uuid>,
    pub game_id: Option<Uuid>,
    pub attachments: Vec<Attachment>,
}

pub struct EvidenceWorkspace {
    _temporary_directory: TempDir,
    pub root: PathBuf,
    pub evidence_dir: PathBuf,
    pub manifest_json: String,
    pub report: Option<BugReport>,
}

#[derive(Serialize)]
struct ManifestEntry {
    path: String,
    bytes: u64,
    source: String,
}

struct ExtractedArchive {
    files: Vec<(String, u64)>,
    entry_count: usize,
    expanded_bytes: u64,
}

struct ArchiveBudget {
    remaining_entries: usize,
    remaining_bytes: u64,
}

impl ArchiveBudget {
    const fn new(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            remaining_entries: max_entries,
            remaining_bytes: max_bytes,
        }
    }

    async fn extract(
        &mut self,
        archive_path: PathBuf,
        destination: PathBuf,
    ) -> Result<Vec<(String, u64)>> {
        let extracted = extract_zip(
            archive_path,
            destination,
            self.remaining_entries,
            self.remaining_bytes,
        )
        .await?;
        self.remaining_entries = self
            .remaining_entries
            .checked_sub(extracted.entry_count)
            .context("archive entry accounting underflow")?;
        self.remaining_bytes = self
            .remaining_bytes
            .checked_sub(extracted.expanded_bytes)
            .context("archive byte accounting underflow")?;
        Ok(extracted.files)
    }
}

#[derive(Clone)]
pub struct EvidenceCollector {
    http: Client,
    shieldbattery: Option<ShieldBatteryClient>,
    config: Arc<Config>,
}

impl EvidenceCollector {
    pub fn new(config: Arc<Config>) -> Result<Self> {
        let http = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .user_agent(concat!("adjutant/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to create evidence HTTP client")?;
        let shieldbattery = config
            .shieldbattery_internal_url
            .clone()
            .map(ShieldBatteryClient::new)
            .transpose()?;
        Ok(Self {
            http,
            shieldbattery,
            config,
        })
    }

    pub async fn collect(
        &self,
        run_id: Uuid,
        request: &EvidenceRequest,
    ) -> Result<EvidenceWorkspace> {
        let temporary_directory = tempfile::Builder::new()
            .prefix(&format!("adjutant-{run_id}-"))
            .tempdir()
            .context("failed to create evidence workspace")?;
        let root = temporary_directory.path().to_path_buf();
        let evidence_dir = root.join("evidence");
        tokio::fs::create_dir_all(&evidence_dir).await?;
        tokio::fs::write(
            root.join("request.md"),
            format!(
                "# Request\n\nFrom: {}\n\n{}\n",
                request.author, request.text
            ),
        )
        .await?;

        let mut manifest = Vec::new();
        let mut report = None;
        let mut archive_budget = ArchiveBudget::new(
            self.config.max_archive_files,
            self.config.max_expanded_bytes,
        );
        if let Some(report_id) = request.bug_report_id {
            let (fetched, entries) = self
                .collect_bug_report(report_id, &root, &evidence_dir, &mut archive_budget)
                .await?;
            manifest.extend(entries);
            report = Some(fetched);
        }

        if let Some(game_id) = request.game_id {
            manifest.extend(self.collect_game_artifacts(game_id, &evidence_dir).await?);
        }

        for (index, attachment) in request.attachments.iter().enumerate() {
            manifest.extend(
                self.collect_attachment(
                    index,
                    attachment,
                    &root,
                    &evidence_dir,
                    &mut archive_budget,
                )
                .await?,
            );
        }

        let manifest_json = serde_json::to_string_pretty(&manifest)?;
        tokio::fs::write(evidence_dir.join("manifest.json"), &manifest_json).await?;
        Ok(EvidenceWorkspace {
            _temporary_directory: temporary_directory,
            root,
            evidence_dir,
            manifest_json,
            report,
        })
    }

    async fn collect_bug_report(
        &self,
        report_id: Uuid,
        root: &Path,
        evidence_dir: &Path,
        archive_budget: &mut ArchiveBudget,
    ) -> Result<(BugReport, Vec<ManifestEntry>)> {
        let client = self.shieldbattery.as_ref().context(
            "automatic bug reports require SHIELDBATTERY_INTERNAL_URL for the ShieldBattery internal API",
        )?;
        let report = client.get_report(report_id).await?;
        let metadata = serde_json::to_string_pretty(&report)?;
        tokio::fs::write(evidence_dir.join("bug-report.json"), &metadata).await?;
        let mut manifest = vec![ManifestEntry {
            path: "bug-report.json".to_owned(),
            bytes: u64::try_from(metadata.len())?,
            source: "ShieldBattery internal API".to_owned(),
        }];
        if report.logs_deleted {
            bail!("ShieldBattery has already deleted the logs for report {report_id}");
        }

        let archive_path = root.join("bug-report.zip");
        self.download_response(client.get_logs(report_id).await?, &archive_path, None)
            .await?;
        let entries = archive_budget
            .extract(archive_path.clone(), evidence_dir.join("bug-report-logs"))
            .await?;
        manifest.extend(entries.into_iter().map(|(path, bytes)| ManifestEntry {
            path: format!("bug-report-logs/{path}"),
            bytes,
            source: format!("ShieldBattery report {report_id}"),
        }));
        tokio::fs::remove_file(archive_path).await?;
        Ok((report, manifest))
    }

    async fn collect_game_artifacts(
        &self,
        game_id: Uuid,
        evidence_dir: &Path,
    ) -> Result<Vec<ManifestEntry>> {
        let client = self.shieldbattery.as_ref().context(
            "game artifact collection requires SHIELDBATTERY_INTERNAL_URL for the ShieldBattery internal API",
        )?;
        let artifacts: GameArtifacts = client.get_game_artifacts(game_id).await?;
        let artifact_count = artifacts
            .flight_recordings
            .len()
            .checked_add(artifacts.replays.len())
            .and_then(|count| count.checked_add(usize::from(artifacts.map.is_some())))
            .context("game artifact count overflow")?;
        if artifact_count > self.config.max_game_artifacts {
            bail!(
                "game {game_id} has {artifact_count} artifacts, over the {} artifact limit",
                self.config.max_game_artifacts
            );
        }

        for replay in &artifacts.replays {
            if replay.size > self.config.max_game_artifact_bytes {
                bail!(
                    "replay {} is {} bytes, over the {} byte per-artifact limit",
                    replay.id,
                    replay.size,
                    self.config.max_game_artifact_bytes
                );
            }
        }

        let directory_name = format!("game-{game_id}");
        let game_directory = evidence_dir.join(&directory_name);
        tokio::fs::create_dir_all(&game_directory).await?;
        let metadata = serde_json::to_string_pretty(&artifacts)?;
        let metadata_bytes = u64::try_from(metadata.len())?;
        if metadata_bytes > self.config.max_game_evidence_bytes {
            bail!(
                "game artifact metadata exceeds the {} byte total game evidence limit",
                self.config.max_game_evidence_bytes
            );
        }
        tokio::fs::write(game_directory.join("artifacts.json"), &metadata).await?;

        let mut remaining_bytes = self
            .config
            .max_game_evidence_bytes
            .checked_sub(metadata_bytes)
            .context("game evidence byte accounting underflow")?;
        let mut manifest = vec![ManifestEntry {
            path: format!("{directory_name}/artifacts.json"),
            bytes: metadata_bytes,
            source: format!("ShieldBattery game {game_id} artifact manifest"),
        }];

        manifest.extend(
            self.collect_flight_recordings(
                client,
                game_id,
                &artifacts,
                &game_directory,
                &directory_name,
                &mut remaining_bytes,
            )
            .await?,
        );

        manifest.extend(
            self.collect_replays(
                client,
                game_id,
                &artifacts,
                &game_directory,
                &directory_name,
                &mut remaining_bytes,
            )
            .await?,
        );

        if let Some(map_entry) = self
            .collect_map(
                client,
                game_id,
                &artifacts,
                &game_directory,
                &directory_name,
                &mut remaining_bytes,
            )
            .await?
        {
            manifest.push(map_entry);
        }

        Ok(manifest)
    }

    async fn collect_flight_recordings(
        &self,
        client: &ShieldBatteryClient,
        game_id: Uuid,
        artifacts: &GameArtifacts,
        game_directory: &Path,
        directory_name: &str,
        remaining_bytes: &mut u64,
    ) -> Result<Vec<ManifestEntry>> {
        if !artifacts.flight_recordings.is_empty() {
            tokio::fs::create_dir_all(game_directory.join("flight-recordings")).await?;
        }
        let mut manifest = Vec::with_capacity(artifacts.flight_recordings.len());
        for flight in &artifacts.flight_recordings {
            let filename = format!("relay-{}.json", flight.relay_id);
            let path = game_directory.join("flight-recordings").join(&filename);
            let response = client
                .get_flight_recording(game_id, flight.relay_id)
                .await?;
            let Some(response) = response else {
                let marker = format!("relay-{}.unavailable.txt", flight.relay_id);
                manifest.push(
                    self.write_unavailable_marker(
                        &game_directory.join("flight-recordings").join(&marker),
                        format!("{directory_name}/flight-recordings/{marker}"),
                        format!(
                            "ShieldBattery game {game_id} flight recording relay {}",
                            flight.relay_id
                        ),
                        remaining_bytes,
                    )
                    .await?,
                );
                continue;
            };
            let bytes = self
                .download_game_response(response, &path, remaining_bytes, None, None, None)
                .await
                .with_context(|| {
                    format!(
                        "failed to collect flight recording for relay {}",
                        flight.relay_id
                    )
                })?;
            manifest.push(ManifestEntry {
                path: format!("{directory_name}/flight-recordings/{filename}"),
                bytes,
                source: format!(
                    "ShieldBattery game {game_id} flight recording relay {}",
                    flight.relay_id
                ),
            });
        }
        Ok(manifest)
    }

    async fn collect_replays(
        &self,
        client: &ShieldBatteryClient,
        game_id: Uuid,
        artifacts: &GameArtifacts,
        game_directory: &Path,
        directory_name: &str,
        remaining_bytes: &mut u64,
    ) -> Result<Vec<ManifestEntry>> {
        if !artifacts.replays.is_empty() {
            tokio::fs::create_dir_all(game_directory.join("replays")).await?;
        }
        let mut manifest = Vec::with_capacity(artifacts.replays.len());
        for replay in &artifacts.replays {
            let filename = format!("{}.rep", replay.id);
            let path = game_directory.join("replays").join(&filename);
            let response = client.get_replay(game_id, replay.id).await?;
            let Some(response) = response else {
                let marker = format!("{}.unavailable.txt", replay.id);
                manifest.push(
                    self.write_unavailable_marker(
                        &game_directory.join("replays").join(&marker),
                        format!("{directory_name}/replays/{marker}"),
                        format!("ShieldBattery game {game_id} replay {}", replay.id),
                        remaining_bytes,
                    )
                    .await?,
                );
                continue;
            };
            let bytes = self
                .download_game_response(
                    response,
                    &path,
                    remaining_bytes,
                    Some(replay.size),
                    Some(&replay.sha256),
                    None,
                )
                .await
                .with_context(|| format!("failed to collect replay {}", replay.id))?;
            manifest.push(ManifestEntry {
                path: format!("{directory_name}/replays/{filename}"),
                bytes,
                source: format!("ShieldBattery game {game_id} replay {}", replay.id),
            });
        }
        Ok(manifest)
    }

    async fn collect_map(
        &self,
        client: &ShieldBatteryClient,
        game_id: Uuid,
        artifacts: &GameArtifacts,
        game_directory: &Path,
        directory_name: &str,
        remaining_bytes: &mut u64,
    ) -> Result<Option<ManifestEntry>> {
        let Some(map) = &artifacts.map else {
            return Ok(None);
        };
        tokio::fs::create_dir_all(game_directory.join("map")).await?;
        let filename = format!("{}.{}", map.hash, map.format.as_str());
        let path = game_directory.join("map").join(&filename);
        let response = client.get_map(game_id).await?;
        let Some(response) = response else {
            return self
                .write_unavailable_marker(
                    &game_directory.join("map/unavailable.txt"),
                    format!("{directory_name}/map/unavailable.txt"),
                    format!("ShieldBattery game {game_id} map {}", map.id),
                    remaining_bytes,
                )
                .await
                .map(Some);
        };
        let bytes = self
            .download_game_response(
                response,
                &path,
                remaining_bytes,
                None,
                Some(&map.hash),
                Some(map.format.as_str()),
            )
            .await
            .context("failed to collect map")?;
        Ok(Some(ManifestEntry {
            path: format!("{directory_name}/map/{filename}"),
            bytes,
            source: format!("ShieldBattery game {game_id} map {}", map.id),
        }))
    }

    async fn write_unavailable_marker(
        &self,
        destination: &Path,
        manifest_path: String,
        source: String,
        remaining_bytes: &mut u64,
    ) -> Result<ManifestEntry> {
        const MESSAGE: &str = "ShieldBattery listed this artifact, but its download returned 404. It may have expired after the manifest was generated.\n";
        let bytes = u64::try_from(MESSAGE.len())?;
        if bytes > self.config.max_game_artifact_bytes {
            bail!("unavailable marker exceeds the per-artifact byte limit");
        }
        if bytes > *remaining_bytes {
            bail!("unavailable marker exceeds the remaining game evidence byte limit");
        }
        tokio::fs::write(destination, MESSAGE).await?;
        *remaining_bytes = remaining_bytes
            .checked_sub(bytes)
            .context("game evidence byte accounting underflow")?;
        Ok(ManifestEntry {
            path: manifest_path,
            bytes,
            source: format!("{source} (unavailable after listing)"),
        })
    }

    async fn download_game_response(
        &self,
        response: Response,
        destination: &Path,
        remaining_bytes: &mut u64,
        expected_size: Option<u64>,
        expected_hash: Option<&str>,
        hash_prefix: Option<&str>,
    ) -> Result<u64> {
        let per_artifact_limit = self.config.max_game_artifact_bytes;
        if let Some(expected) = expected_size {
            if expected > per_artifact_limit {
                bail!(
                    "artifact is {expected} bytes, over the {per_artifact_limit} byte per-artifact limit"
                );
            }
            if expected > *remaining_bytes {
                bail!("artifact exceeds the remaining game evidence byte limit");
            }
        }
        if let Some(length) = response.content_length() {
            if length > per_artifact_limit {
                bail!(
                    "artifact is {length} bytes, over the {per_artifact_limit} byte per-artifact limit"
                );
            }
            if length > *remaining_bytes {
                bail!("artifact exceeds the remaining game evidence byte limit");
            }
            if expected_size.is_some_and(|expected| expected != length) {
                bail!("artifact Content-Length does not match its manifest size");
            }
        }

        let mut file = tokio::fs::File::create(destination)
            .await
            .with_context(|| format!("failed to create {}", destination.display()))?;
        let mut hasher = expected_hash.map(|_| Sha256::new());
        if let (Some(hasher), Some(prefix)) = (&mut hasher, hash_prefix) {
            hasher.update(prefix.as_bytes());
        }
        let mut stream = response.bytes_stream();
        let mut downloaded = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("game artifact download failed")?;
            downloaded = downloaded
                .checked_add(u64::try_from(chunk.len())?)
                .context("game artifact size overflow")?;
            if downloaded > per_artifact_limit {
                bail!("artifact exceeded the {per_artifact_limit} byte per-artifact limit");
            }
            if downloaded > *remaining_bytes {
                bail!("artifact exceeded the remaining game evidence byte limit");
            }
            if let Some(hasher) = &mut hasher {
                hasher.update(&chunk);
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;

        if expected_size.is_some_and(|expected| expected != downloaded) {
            bail!("artifact size does not match its manifest size");
        }
        if let (Some(hasher), Some(expected)) = (hasher, expected_hash) {
            let actual = finish_sha256_hex(hasher);
            if actual != expected {
                bail!("artifact SHA-256 does not match its manifest hash");
            }
        }
        *remaining_bytes = remaining_bytes
            .checked_sub(downloaded)
            .context("game evidence byte accounting underflow")?;
        Ok(downloaded)
    }

    async fn collect_attachment(
        &self,
        index: usize,
        attachment: &Attachment,
        root: &Path,
        evidence_dir: &Path,
        archive_budget: &mut ArchiveBudget,
    ) -> Result<Vec<ManifestEntry>> {
        validate_discord_attachment(attachment)?;
        if attachment.size > self.config.max_download_bytes {
            bail!(
                "attachment {:?} is {} bytes, over the {} byte limit",
                attachment.filename,
                attachment.size,
                self.config.max_download_bytes
            );
        }
        let filename = format!("{index:02}-{}", safe_filename(&attachment.filename));
        let is_zip = attachment.filename.to_ascii_lowercase().ends_with(".zip")
            || attachment.content_type.as_deref() == Some("application/zip");
        let path = if is_zip {
            root.join(&filename)
        } else {
            evidence_dir.join(&filename)
        };
        let response = self
            .http
            .get(attachment.url.clone())
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to download Discord attachment {:?}",
                    attachment.filename
                )
            })?;
        if !response.status().is_success() {
            bail!(
                "Discord attachment {:?} returned {}",
                attachment.filename,
                response.status()
            );
        }
        let bytes = self
            .download_response(response, &path, Some(attachment.size))
            .await?;
        if !is_zip {
            return Ok(vec![ManifestEntry {
                path: filename,
                bytes,
                source: format!("Discord attachment {}", attachment.filename),
            }]);
        }

        let directory_name = filename.trim_end_matches(".zip");
        let entries = archive_budget
            .extract(path.clone(), evidence_dir.join(directory_name))
            .await?;
        tokio::fs::remove_file(path).await?;
        Ok(entries
            .into_iter()
            .map(|(entry, bytes)| ManifestEntry {
                path: format!("{directory_name}/{entry}"),
                bytes,
                source: format!("Discord attachment {}", attachment.filename),
            })
            .collect())
    }

    async fn download_response(
        &self,
        response: Response,
        destination: &Path,
        expected_size: Option<u64>,
    ) -> Result<u64> {
        if let Some(length) = response.content_length()
            && length > self.config.max_download_bytes
        {
            bail!(
                "download is {length} bytes, over the {} byte limit",
                self.config.max_download_bytes
            );
        }
        let mut file = tokio::fs::File::create(destination)
            .await
            .with_context(|| format!("failed to create {}", destination.display()))?;
        let mut stream = response.bytes_stream();
        let mut downloaded = 0_u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("evidence download failed")?;
            downloaded = downloaded.saturating_add(u64::try_from(chunk.len())?);
            if downloaded > self.config.max_download_bytes {
                bail!(
                    "download exceeded the {} byte limit",
                    self.config.max_download_bytes
                );
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        if let Some(expected) = expected_size
            && downloaded != expected
        {
            bail!("downloaded {downloaded} bytes but Discord declared {expected} bytes");
        }
        Ok(downloaded)
    }
}

fn finish_sha256_hex(hasher: Sha256) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    hasher
        .finalize()
        .iter()
        .flat_map(|byte| {
            [
                char::from(HEX[usize::from(*byte >> 4)]),
                char::from(HEX[usize::from(*byte & 0x0f)]),
            ]
        })
        .collect()
}
fn validate_discord_attachment(attachment: &Attachment) -> Result<()> {
    let host = attachment.url.host_str().unwrap_or_default();
    if attachment.url.scheme() != "https"
        || !(host == "cdn.discordapp.com"
            || host.ends_with(".discordapp.com")
            || host == "media.discordapp.net"
            || host.ends_with(".discordapp.net"))
    {
        bail!("Discord attachment URL is not on an approved Discord CDN host");
    }
    Ok(())
}

fn safe_filename(filename: &str) -> String {
    let base = Path::new(filename)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("attachment");
    let sanitized: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    if sanitized.is_empty() {
        "attachment".to_owned()
    } else {
        sanitized
    }
}

async fn extract_zip(
    archive_path: PathBuf,
    destination: PathBuf,
    max_files: usize,
    max_expanded_bytes: u64,
) -> Result<ExtractedArchive> {
    tokio::task::spawn_blocking(move || {
        extract_zip_blocking(&archive_path, &destination, max_files, max_expanded_bytes)
    })
    .await
    .context("ZIP extraction task failed")?
}

fn extract_zip_blocking(
    archive_path: &Path,
    destination: &Path,
    max_files: usize,
    max_expanded_bytes: u64,
) -> Result<ExtractedArchive> {
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file).context("attachment is not a readable ZIP archive")?;
    if archive.len() > max_files {
        bail!(
            "ZIP contains {} entries, over the {max_files} entry limit",
            archive.len()
        );
    }
    std::fs::create_dir_all(destination)?;
    let mut total = 0_u64;
    let mut extracted = Vec::new();

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let relative = entry
            .enclosed_name()
            .context("ZIP contains an unsafe path")?
            .clone();
        if relative.components().count() > MAX_ARCHIVE_PATH_DEPTH
            || !relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            bail!("ZIP entry path is too deep or invalid");
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170_000 == 0o120_000)
        {
            bail!("ZIP contains a symbolic link");
        }
        if entry.is_dir() {
            std::fs::create_dir_all(destination.join(&relative))?;
            continue;
        }

        let remaining = max_expanded_bytes.saturating_sub(total);
        if entry.size() > remaining {
            bail!("ZIP expands beyond the {max_expanded_bytes} byte limit");
        }
        let output_path = destination.join(&relative);
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output_path)
            .with_context(|| format!("duplicate or invalid ZIP entry {}", relative.display()))?;
        let copied = std::io::copy(
            &mut (&mut entry).take(remaining.saturating_add(1)),
            &mut output,
        )?;
        output.flush()?;
        if copied > remaining {
            bail!("ZIP expands beyond the {max_expanded_bytes} byte limit");
        }
        total = total.saturating_add(copied);
        extracted.push((relative.to_string_lossy().replace('\\', "/"), copied));
    }
    Ok(ExtractedArchive {
        files: extracted,
        entry_count: archive.len(),
        expanded_bytes: total,
    })
}

#[cfg(test)]
mod tests {
    use axum::{Router, body::Body, routing::get};
    use std::io::Cursor;

    use zip::write::SimpleFileOptions;

    use super::*;

    #[test]
    fn sanitizes_attachment_names() {
        assert_eq!(safe_filename("../../bad name?.zip"), "bad_name_.zip");
        assert_eq!(safe_filename("normal.log"), "normal.log");
    }

    #[test]
    fn rejects_path_traversal_in_zip() {
        let directory = tempfile::tempdir().unwrap();
        let archive_path = directory.path().join("bad.zip");
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            writer
                .start_file("../escaped.log", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"nope").unwrap();
            writer.finish().unwrap();
        }
        std::fs::write(&archive_path, bytes.into_inner()).unwrap();

        assert!(
            extract_zip_blocking(&archive_path, &directory.path().join("out"), 10, 1024).is_err()
        );
        assert!(!directory.path().join("escaped.log").exists());
    }

    #[test]
    fn enforces_expanded_size_limit() {
        let directory = tempfile::tempdir().unwrap();
        let archive_path = directory.path().join("large.zip");
        let mut bytes = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            writer
                .start_file("large.log", SimpleFileOptions::default())
                .unwrap();
            writer.write_all(&[0; 128]).unwrap();
            writer.finish().unwrap();
        }
        std::fs::write(&archive_path, bytes.into_inner()).unwrap();

        assert!(
            extract_zip_blocking(&archive_path, &directory.path().join("out"), 10, 64).is_err()
        );
    }

    #[tokio::test]
    async fn enforces_archive_limits_across_all_job_attachments() {
        let directory = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for index in 0..2 {
            let archive_path = directory.path().join(format!("{index}.zip"));
            let mut bytes = Cursor::new(Vec::new());
            {
                let mut writer = zip::ZipWriter::new(&mut bytes);
                writer
                    .start_file(format!("{index}.log"), SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(b"evidence").unwrap();
                writer.finish().unwrap();
            }
            std::fs::write(&archive_path, bytes.into_inner()).unwrap();
            paths.push(archive_path);
        }

        let mut budget = ArchiveBudget::new(1, 1024);
        budget
            .extract(paths[0].clone(), directory.path().join("out-0"))
            .await
            .unwrap();
        assert!(
            budget
                .extract(paths[1].clone(), directory.path().join("out-1"))
                .await
                .is_err()
        );
    }

    fn test_config(internal_url: Url) -> Arc<Config> {
        Arc::new(Config {
            discord_token: "test".to_owned(),
            discord_guild_id: 1,
            discord_bug_report_channel_id: 2,
            discord_bug_report_webhook_id: 3,
            discord_request_channel_id: 4,
            discord_output_channel_id: 5,
            discord_allowed_role_ids: std::collections::HashSet::new(),
            shieldbattery_public_url: Url::parse("https://shieldbattery.net").unwrap(),
            shieldbattery_internal_url: Some(internal_url),
            codex_bin: "codex".to_owned(),
            codex_home: PathBuf::from(".codex"),
            codex_profile: None,
            codex_model: None,
            shieldbattery_source_dir: PathBuf::from("source"),
            codex_env_passthrough: Vec::new(),
            max_concurrent_jobs: 1,
            max_queued_jobs: 1,
            max_download_bytes: 1024,
            max_game_artifacts: 4,
            max_game_artifact_bytes: 1024 * 1024,
            max_game_evidence_bytes: 4 * 1024 * 1024,
            max_archive_files: 4,
            max_expanded_bytes: 4096,
            max_codex_events: 10,
            max_codex_event_bytes: 4096,
            max_codex_event_line_bytes: 1024,
            job_timeout: Duration::from_secs(30),
            database_path: PathBuf::from("adjutant.sqlite3"),
            ui_bind: "127.0.0.1:0".parse().unwrap(),
            ui_base_url: None,
            run_retention_days: 1,
        })
    }

    fn test_response(bytes: Vec<u8>, content_type: &'static str) -> axum::http::Response<Body> {
        axum::http::Response::builder()
            .header(axum::http::header::CONTENT_TYPE, content_type)
            .header(axum::http::header::CONTENT_LENGTH, bytes.len().to_string())
            .body(Body::from(bytes))
            .unwrap()
    }

    fn test_hash(prefix: Option<&str>, bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        if let Some(prefix) = prefix {
            hasher.update(prefix.as_bytes());
        }
        hasher.update(bytes);
        finish_sha256_hex(hasher)
    }

    #[tokio::test]
    // Keeping the whole wire contract visible in one test makes endpoint drift easier to review.
    #[allow(clippy::too_many_lines)]
    async fn collects_all_game_artifacts_over_the_internal_api() {
        let game_id = Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0ba").unwrap();
        let replay_id = Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0bb").unwrap();
        let map_id = Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0bc").unwrap();
        let flight_bytes = br#"{"events":[{"frame":42}]}"#.to_vec();
        let replay_bytes = b"replay bytes".to_vec();
        let map_bytes = b"map bytes".to_vec();
        let replay_hash = test_hash(None, &replay_bytes);
        let map_hash = test_hash(Some("scx"), &map_bytes);
        let manifest_bytes = serde_json::to_vec(&serde_json::json!({
            "gameId": game_id,
            "flightRecordings": [{
                "relayId": 7,
                "pinned": true,
                "size": 10,
                "lastModifiedMs": 1_700_000_000_000_i64,
                "downloadPath": format!(
                    "/internal/games/{game_id}/artifacts/flight-recordings/7"
                ),
            }, {
                "relayId": 8,
                "pinned": false,
                "size": 10,
                "lastModifiedMs": 1_700_000_000_001_i64,
                "downloadPath": format!(
                    "/internal/games/{game_id}/artifacts/flight-recordings/8"
                ),
            }],
            "replays": [{
                "id": replay_id,
                "uploaderUserId": 9,
                "size": replay_bytes.len(),
                "sha256": replay_hash,
                "frames": 1234,
                "downloadPath": format!(
                    "/internal/games/{game_id}/artifacts/replays/{replay_id}"
                ),
            }],
            "map": {
                "id": map_id,
                "hash": map_hash,
                "format": "scx",
                "name": "Fighting Spirit",
                "downloadPath": format!("/internal/games/{game_id}/artifacts/map"),
            },
        }))
        .unwrap();

        let manifest_route = format!("/internal/games/{game_id}/artifacts");
        let flight_route = format!("/internal/games/{game_id}/artifacts/flight-recordings/7");
        let replay_route = format!("/internal/games/{game_id}/artifacts/replays/{replay_id}");
        let map_route = format!("/internal/games/{game_id}/artifacts/map");
        let app = Router::new()
            .route(
                &manifest_route,
                get(move || {
                    let bytes = manifest_bytes.clone();
                    async move { test_response(bytes, "application/json") }
                }),
            )
            .route(
                &flight_route,
                get({
                    let flight_bytes = flight_bytes.clone();
                    move || {
                        let bytes = flight_bytes.clone();
                        async move { test_response(bytes, "application/json") }
                    }
                }),
            )
            .route(
                &replay_route,
                get({
                    let replay_bytes = replay_bytes.clone();
                    move || {
                        let bytes = replay_bytes.clone();
                        async move { test_response(bytes, "application/octet-stream") }
                    }
                }),
            )
            .route(
                &map_route,
                get({
                    let map_bytes = map_bytes.clone();
                    move || {
                        let bytes = map_bytes.clone();
                        async move { test_response(bytes, "application/octet-stream") }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let internal_url = Url::parse(&format!("http://{address}")).unwrap();
        let collector = EvidenceCollector::new(test_config(internal_url.clone())).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let evidence_dir = directory.path().join("evidence");
        tokio::fs::create_dir(&evidence_dir).await.unwrap();
        let entries = collector
            .collect_game_artifacts(game_id, &evidence_dir)
            .await
            .unwrap();

        let game_directory = evidence_dir.join(format!("game-{game_id}"));
        assert_eq!(entries.len(), 5);
        assert_eq!(
            tokio::fs::read(game_directory.join("flight-recordings/relay-7.json"))
                .await
                .unwrap(),
            flight_bytes,
        );
        assert!(
            tokio::fs::read_to_string(
                game_directory.join("flight-recordings/relay-8.unavailable.txt")
            )
            .await
            .unwrap()
            .contains("returned 404"),
        );
        assert_eq!(
            tokio::fs::read(game_directory.join(format!("replays/{replay_id}.rep")))
                .await
                .unwrap(),
            replay_bytes,
        );
        assert_eq!(
            tokio::fs::read(game_directory.join(format!("map/{map_hash}.scx")))
                .await
                .unwrap(),
            map_bytes,
        );
        assert!(game_directory.join("artifacts.json").is_file());

        let replay_url = internal_url.join(&replay_route).unwrap();
        let wrong_hash = "00".repeat(32);
        let response = collector.http.get(replay_url.clone()).send().await.unwrap();
        let mut remaining_bytes = 1024;
        let hash_error = collector
            .download_game_response(
                response,
                &directory.path().join("wrong-hash.rep"),
                &mut remaining_bytes,
                Some(u64::try_from(replay_bytes.len()).unwrap()),
                Some(&wrong_hash),
                None,
            )
            .await
            .unwrap_err();
        assert!(hash_error.to_string().contains("SHA-256"));

        let mut limited_config = (*test_config(internal_url)).clone();
        limited_config.max_game_artifact_bytes = 1;
        limited_config.max_game_evidence_bytes = 1024;
        let limited_collector = EvidenceCollector::new(Arc::new(limited_config)).unwrap();
        let response = limited_collector
            .http
            .get(replay_url.clone())
            .send()
            .await
            .unwrap();
        let mut remaining_bytes = 1024;
        let size_error = limited_collector
            .download_game_response(
                response,
                &directory.path().join("over-limit.rep"),
                &mut remaining_bytes,
                Some(u64::try_from(replay_bytes.len()).unwrap()),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(size_error.to_string().contains("per-artifact limit"));

        let response = collector.http.get(replay_url).send().await.unwrap();
        let mut remaining_bytes = 1;
        let total_error = collector
            .download_game_response(
                response,
                &directory.path().join("over-total.rep"),
                &mut remaining_bytes,
                Some(u64::try_from(replay_bytes.len()).unwrap()),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(total_error.to_string().contains("remaining game evidence"));
        server.abort();
    }
}
