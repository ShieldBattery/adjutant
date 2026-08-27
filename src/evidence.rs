use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, Response, redirect::Policy};
use serde::Serialize;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use url::Url;
use uuid::Uuid;
use zip::ZipArchive;

use crate::config::Config;
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
            .zip(config.shieldbattery_internal_token.clone())
            .map(|(url, token)| ShieldBatteryClient::new(url, token))
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
            "automatic bug reports require SHIELDBATTERY_INTERNAL_URL and SHIELDBATTERY_INTERNAL_TOKEN; see docs/shieldbattery-internal-api.md",
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
}
