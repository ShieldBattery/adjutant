//! Maintains read-only, generation-consistent source snapshots for Codex.

use std::{
    collections::BTreeSet,
    env,
    ffi::OsStr,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::{Client, Response, redirect::Policy};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncReadExt, process::Command, time::sleep};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

const API_BASE: &str = "https://api.github.com";
const GITHUB_BASE: &str = "https://github.com";
const PRIMARY_REPOSITORY: &str = "ShieldBattery";
const PAGE_SIZE: usize = 100;
const MAX_API_BODY_BYTES: u64 = 2 * 1024 * 1024;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_mins(15);
const READY_FILE: &str = ".adjutant-source-ready";
const MANIFEST_FILE: &str = ".adjutant-source-manifest.json";
const RETIRED_FILE: &str = ".adjutant-source-retired";
const STAGING_PREFIX: &str = ".staging-";

#[derive(Debug)]
struct Config {
    root: PathBuf,
    organization: String,
    interval: Duration,
    git_depth: u32,
    retention: Duration,
    max_repositories: usize,
    max_repository_kib: u64,
    max_total_kib: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubRepositoryResponse {
    name: String,
    default_branch: Option<String>,
    size: u64,
}

#[derive(Debug, Clone)]
struct GithubRepository {
    name: String,
    default_branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ManifestRepository {
    name: String,
    default_branch: String,
    commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Manifest {
    organization: String,
    generated_at: u64,
    repositories: Vec<ManifestRepository>,
}

struct ProcessGroupGuard {
    #[cfg(target_os = "linux")]
    process_id: u32,
    armed: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    initialize_tracing();
    let args: Vec<String> = env::args().skip(1).collect();
    let config = Config::from_env()?;

    match args.as_slice() {
        [] => run(config).await,
        [command] if command == "healthcheck" => healthcheck(&config),
        _ => bail!("usage: adjutant-source-sync [healthcheck]"),
    }
}

impl Config {
    fn from_env() -> Result<Self> {
        let root = env::var_os("SOURCE_SYNC_ROOT")
            .filter(|value| !value.is_empty())
            .map_or_else(|| PathBuf::from("/workspace/repos"), PathBuf::from);
        let organization =
            env::var("SOURCE_SYNC_GITHUB_ORG").unwrap_or_else(|_| "ShieldBattery".to_owned());
        ensure!(
            is_safe_owner(&organization),
            "invalid SOURCE_SYNC_GITHUB_ORG"
        );
        let interval_seconds = bounded_env("SOURCE_SYNC_INTERVAL_SECONDS", 900, 300, 86_400)?;
        let git_depth = bounded_env("SOURCE_SYNC_GIT_DEPTH", 200, 1, 10_000)?;
        let retention_seconds = bounded_env("SOURCE_SYNC_RETENTION_SECONDS", 7200, 300, 2_592_000)?;
        let job_timeout_seconds = bounded_env("SOURCE_SYNC_JOB_TIMEOUT_SECONDS", 1800, 60, 86_400)?;
        let max_repositories = bounded_env("SOURCE_SYNC_MAX_REPOSITORIES", 100, 1, 500)?;
        let max_repository_kib =
            bounded_env("SOURCE_SYNC_MAX_REPOSITORY_KIB", 1_048_576, 1, 104_857_600)?;
        let max_total_kib = bounded_env("SOURCE_SYNC_MAX_TOTAL_KIB", 4_194_304, 1, 104_857_600)?;
        ensure!(
            retention_seconds >= job_timeout_seconds.saturating_add(interval_seconds),
            "SOURCE_SYNC_RETENTION_SECONDS must be at least SOURCE_SYNC_JOB_TIMEOUT_SECONDS plus SOURCE_SYNC_INTERVAL_SECONDS"
        );
        ensure!(
            max_total_kib >= max_repository_kib,
            "SOURCE_SYNC_MAX_TOTAL_KIB must be at least SOURCE_SYNC_MAX_REPOSITORY_KIB"
        );
        Ok(Self {
            root,
            organization,
            interval: Duration::from_secs(interval_seconds),
            git_depth: u32::try_from(git_depth).context("SOURCE_SYNC_GIT_DEPTH is too large")?,
            retention: Duration::from_secs(retention_seconds),
            max_repositories: usize::try_from(max_repositories)
                .context("SOURCE_SYNC_MAX_REPOSITORIES is too large")?,
            max_repository_kib,
            max_total_kib,
        })
    }
}

fn bounded_env(name: &str, default: u64, minimum: u64, maximum: u64) -> Result<u64> {
    let value = env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<u64>()
                .with_context(|| format!("invalid {name}"))
        })
        .transpose()?
        .unwrap_or(default);
    ensure!(
        value >= minimum && value <= maximum,
        "{name} must be {minimum}..={maximum}"
    );
    Ok(value)
}

async fn run(config: Config) -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("adjutant-source-sync/0.1")
        .redirect(Policy::none())
        .build()
        .context("building GitHub API client")?;
    loop {
        let delay = tokio::select! {
            result = sync_once(&config, &client) => match result {
                Ok(()) => config.interval,
                Err(error) => {
                    warn!(error = %error, "source synchronization failed; preserving prior snapshot");
                    config
                        .interval
                        .min(Duration::from_secs(60))
                        .max(Duration::from_secs(5))
                }
            },
            result = wait_for_shutdown() => {
                result?;
                info!("source synchronizer stopped");
                return Ok(());
            }
        };
        tokio::select! {
            () = sleep(delay) => {},
            result = wait_for_shutdown() => {
                result?;
                info!("source synchronizer stopped");
                return Ok(());
            }
        }
    }
}

async fn sync_once(config: &Config, client: &Client) -> Result<()> {
    ensure_layout(&config.root)?;
    let repositories = list_repositories(
        client,
        &config.organization,
        config.max_repositories,
        config.max_repository_kib,
        config.max_total_kib,
    )
    .await?;
    ensure!(
        repositories
            .iter()
            .any(|repository| repository.name == PRIMARY_REPOSITORY),
        "primary repository {PRIMARY_REPOSITORY:?} was not found or is empty"
    );
    let mut recorded = Vec::with_capacity(repositories.len());
    for repository in repositories {
        let commit = update_mirror(config, &repository).await?;
        recorded.push(ManifestRepository {
            name: repository.name,
            default_branch: repository.default_branch,
            commit,
        });
    }
    recorded.sort_by(|left, right| left.name.cmp(&right.name));
    let manifest = Manifest {
        organization: config.organization.clone(),
        generated_at: unix_timestamp()?,
        repositories: recorded,
    };
    if current_manifest_matches(&config.root, &manifest)? {
        info!(
            repositories = manifest.repositories.len(),
            "source snapshot is already current"
        );
        touch_readiness(
            &config.root,
            current_generation_id(&config.root)?.as_deref(),
        )?;
        cleanup_generations(&config.root, config.retention)?;
        cleanup_mirrors(&config.root, &manifest)?;
        return Ok(());
    }
    publish_generation(config, &manifest).await?;
    cleanup_generations(&config.root, config.retention)?;
    cleanup_mirrors(&config.root, &manifest)?;
    info!(
        repositories = manifest.repositories.len(),
        "published source snapshot"
    );
    Ok(())
}

fn ensure_layout(root: &Path) -> Result<()> {
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    fs::create_dir_all(root.join("mirrors"))?;
    let generations = root.join("generations");
    fs::create_dir_all(&generations)?;
    cleanup_staging_directories(&generations)?;
    Ok(())
}

fn cleanup_staging_directories(generations: &Path) -> Result<()> {
    for entry in fs::read_dir(generations)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_staging_name(name) {
            continue;
        }
        ensure!(
            entry.file_type()?.is_dir(),
            "source staging path is not a directory"
        );
        fs::remove_dir_all(entry.path()).context("removing stale source staging directory")?;
    }
    Ok(())
}

fn cleanup_mirrors(root: &Path, manifest: &Manifest) -> Result<()> {
    let current: BTreeSet<_> = manifest
        .repositories
        .iter()
        .map(|repository| repository.name.as_str())
        .collect();
    for entry in fs::read_dir(root.join("mirrors"))? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(repository) = name.strip_suffix(".git") else {
            continue;
        };
        if !is_safe_repo_name(repository) || current.contains(repository) {
            continue;
        }
        ensure!(
            entry.file_type()?.is_dir(),
            "obsolete source mirror path is not a directory"
        );
        fs::remove_dir_all(entry.path()).context("removing obsolete source mirror")?;
    }
    Ok(())
}

async fn list_repositories(
    client: &Client,
    organization: &str,
    maximum: usize,
    maximum_repository_kib: u64,
    maximum_total_kib: u64,
) -> Result<Vec<GithubRepository>> {
    let mut repositories = Vec::new();
    let mut discovered = 0_usize;
    let mut total_kib = 0_u64;
    for page in 1..=pagination_page_limit(maximum) {
        let url =
            format!("{API_BASE}/orgs/{organization}/repos?type=public&per_page=100&page={page}");
        let response = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("listing GitHub repositories page {page}"))?
            .error_for_status()
            .with_context(|| format!("GitHub rejected repository listing page {page}"))?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_API_BODY_BYTES)
        {
            bail!("GitHub repository listing response exceeds size limit");
        }
        let body = read_response_bounded(response).await?;
        let received: Vec<GithubRepositoryResponse> =
            serde_json::from_slice(&body).context("parsing GitHub repository listing")?;
        ensure!(
            received.len() <= PAGE_SIZE,
            "GitHub returned oversized repository page"
        );
        let received_count = received.len();
        for repository in received {
            discovered = discovered
                .checked_add(1)
                .ok_or_else(|| anyhow!("GitHub repository count overflowed"))?;
            ensure!(
                repository_cap_allows(discovered, maximum),
                "GitHub organization exceeds configured repository cap"
            );
            if repository.size == 0 {
                continue;
            }
            ensure!(
                repository.size <= maximum_repository_kib,
                "GitHub repository {:?} exceeds SOURCE_SYNC_MAX_REPOSITORY_KIB",
                repository.name
            );
            total_kib = total_kib
                .checked_add(repository.size)
                .ok_or_else(|| anyhow!("GitHub repository size total overflowed"))?;
            ensure!(
                total_kib <= maximum_total_kib,
                "GitHub organization exceeds SOURCE_SYNC_MAX_TOTAL_KIB"
            );
            let default_branch = repository
                .default_branch
                .ok_or_else(|| anyhow!("non-empty GitHub repository has no default branch"))?;
            ensure!(
                is_safe_repo_name(&repository.name),
                "unsafe GitHub repository name"
            );
            ensure!(
                is_safe_branch(&default_branch),
                "unsafe GitHub default branch"
            );
            repositories.push(GithubRepository {
                name: repository.name,
                default_branch,
            });
        }
        if received_count < PAGE_SIZE {
            return Ok(repositories);
        }
    }
    bail!("GitHub pagination exceeded repository cap")
}

async fn read_response_bounded(mut response: Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("reading GitHub repository listing")?
    {
        let next_length = body
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| anyhow!("GitHub repository listing response length overflowed"))?;
        ensure!(
            next_length <= usize::try_from(MAX_API_BODY_BYTES)?,
            "GitHub repository listing response exceeds size limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn update_mirror(config: &Config, repository: &GithubRepository) -> Result<String> {
    let mirror = config
        .root
        .join("mirrors")
        .join(format!("{}.git", repository.name));
    let url = format!(
        "{GITHUB_BASE}/{}/{}.git",
        config.organization, repository.name
    );
    if mirror.exists() {
        ensure!(
            fs::metadata(&mirror)?.is_dir(),
            "mirror path is not a directory"
        );
        git([
            "-C",
            path_to_arg(&mirror)?,
            "remote",
            "set-url",
            "origin",
            &url,
        ])
        .await?;
    } else {
        git(["init", "--bare", "--quiet", path_to_arg(&mirror)?]).await?;
        git(["-C", path_to_arg(&mirror)?, "remote", "add", "origin", &url]).await?;
    }
    let branch_ref = format!("+refs/heads/{0}:refs/heads/{0}", repository.default_branch);
    git([
        "-C",
        path_to_arg(&mirror)?,
        "fetch",
        "--quiet",
        "--depth",
        &config.git_depth.to_string(),
        "--no-tags",
        "--prune",
        "--force",
        "origin",
        &branch_ref,
    ])
    .await?;
    let branch = format!("refs/heads/{}", repository.default_branch);
    git(["-C", path_to_arg(&mirror)?, "symbolic-ref", "HEAD", &branch]).await?;
    prune_mirror_refs(&mirror, &branch).await?;
    git([
        "-C",
        path_to_arg(&mirror)?,
        "reflog",
        "expire",
        "--expire=now",
        "--all",
    ])
    .await?;
    git(["-C", path_to_arg(&mirror)?, "gc", "--quiet", "--prune=now"]).await?;
    let commit = git_output(["-C", path_to_arg(&mirror)?, "rev-parse", &branch]).await?;
    let commit = commit.trim().to_owned();
    ensure!(
        is_commit(&commit),
        "git returned an invalid commit identifier"
    );
    Ok(commit)
}

async fn prune_mirror_refs(mirror: &Path, retained_branch: &str) -> Result<()> {
    let refs = git_output([
        "-C",
        path_to_arg(mirror)?,
        "for-each-ref",
        "--format=%(refname)",
        "refs/heads",
        "refs/remotes",
    ])
    .await?;
    for reference in refs.lines() {
        ensure!(
            reference.starts_with("refs/heads/") || reference.starts_with("refs/remotes/"),
            "git returned an unexpected mirror reference"
        );
        if reference != retained_branch {
            git(["-C", path_to_arg(mirror)?, "update-ref", "-d", reference]).await?;
        }
    }
    Ok(())
}

async fn publish_generation(config: &Config, manifest: &Manifest) -> Result<()> {
    let generation_id = Uuid::now_v7().to_string();
    let generations = config.root.join("generations");
    let staging = generations.join(format!("{STAGING_PREFIX}{generation_id}"));
    let final_path = generations.join(&generation_id);
    fs::create_dir(&staging).with_context(|| format!("creating {}", staging.display()))?;
    let result =
        materialize_generation(config, manifest, &generation_id, &staging, &final_path).await;
    if let Err(error) = result {
        if let Err(cleanup_error) = remove_failed_staging(&staging, &generation_id) {
            warn!(path = %staging.display(), error = %cleanup_error, "failed to remove incomplete source generation");
        }
        return Err(error);
    }
    Ok(())
}

async fn materialize_generation(
    config: &Config,
    manifest: &Manifest,
    generation_id: &str,
    staging: &Path,
    final_path: &Path,
) -> Result<()> {
    let previous_generation = current_generation_id(&config.root)?;
    for repository in &manifest.repositories {
        let mirror = config
            .root
            .join("mirrors")
            .join(format!("{}.git", repository.name));
        let destination = staging.join(&repository.name);
        git_local_clone(
            &mirror,
            &destination,
            &repository.default_branch,
            config.git_depth,
        )
        .await?;
        git([
            "-C",
            path_to_arg(&destination)?,
            "checkout",
            "--quiet",
            "--detach",
            "--no-recurse-submodules",
            &repository.commit,
        ])
        .await?;
    }
    write_manifest(staging, manifest)?;
    fs::rename(staging, final_path)
        .with_context(|| format!("finalizing {}", final_path.display()))?;
    replace_current_link(&config.root, generation_id)?;
    touch_readiness(&config.root, Some(generation_id))?;
    if let Some(previous_generation) = previous_generation
        && let Err(error) = mark_generation_retired(&config.root, &previous_generation)
    {
        warn!(generation = %previous_generation, error = %error, "could not mark replaced source generation as retired");
    }
    Ok(())
}

fn remove_failed_staging(staging: &Path, generation_id: &str) -> Result<()> {
    ensure!(
        is_generation_id(generation_id),
        "invalid staging generation ID"
    );
    let expected_name = format!("{STAGING_PREFIX}{generation_id}");
    ensure!(
        staging.file_name() == Some(OsStr::new(&expected_name)),
        "unexpected staging path"
    );
    let metadata = match fs::symlink_metadata(staging) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspecting incomplete source generation"),
    };
    ensure!(
        metadata.file_type().is_dir(),
        "staging path is not a directory"
    );
    fs::remove_dir_all(staging).context("removing incomplete source generation")
}

async fn git<I, S>(args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (status, _stdout, stderr) = run_git(args, false, false).await?;
    if status.success() {
        return Ok(());
    }
    bail!("git failed: {}", bounded_stderr(&stderr));
}

async fn git_output<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (status, stdout, stderr) = run_git(args, true, false).await?;
    if status.success() {
        return String::from_utf8(stdout).context("git returned non-UTF-8 output");
    }
    bail!("git failed: {}", bounded_stderr(&stderr));
}

async fn run_git<I, S>(
    args: I,
    capture_stdout: bool,
    allow_file_protocol: bool,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut child = git_command(args, capture_stdout, allow_file_protocol)
        .spawn()
        .context("starting git")?;
    let process_id = child.id().context("git process has no process ID")?;
    let mut process_group = ProcessGroupGuard::new(process_id);
    let stdout = child.stdout.take();
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("git stderr was unavailable"))?;
    let result = tokio::time::timeout(GIT_COMMAND_TIMEOUT, async {
        tokio::join!(
            child.wait(),
            read_optional_stream(stdout),
            read_stream_bounded(stderr),
        )
    })
    .await;
    let Ok((status, stdout, stderr)) = result else {
        process_group.terminate();
        let _ = child.kill().await;
        let _ = child.wait().await;
        bail!("git command exceeded the {GIT_COMMAND_TIMEOUT:?} timeout");
    };
    process_group.disarm();
    Ok((status.context("waiting for git")?, stdout?, stderr?))
}

async fn read_optional_stream<R>(reader: Option<R>) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(reader) = reader else {
        return Ok(Vec::new());
    };
    read_stream_bounded(reader).await
}

async fn read_stream_bounded<R>(reader: R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(1025);
    reader.take(1025).read_to_end(&mut bytes).await?;
    Ok(bytes)
}

fn git_command<I, S>(args: I, capture_stdout: bool, allow_file_protocol: bool) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg(if allow_file_protocol {
            "protocol.file.allow=always"
        } else {
            "protocol.file.allow=never"
        })
        .arg("-c")
        .arg("submodule.recurse=false")
        .arg("-c")
        .arg("maintenance.auto=false")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("-c")
        .arg("http.followRedirects=false")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_HTTP_LOW_SPEED_LIMIT", "1")
        .env("GIT_HTTP_LOW_SPEED_TIME", "60")
        .stdin(Stdio::null())
        .stdout(if capture_stdout {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::piped());
    command.kill_on_drop(true);
    #[cfg(target_os = "linux")]
    command.process_group(0);
    command
}

async fn git_local_clone(
    mirror: &Path,
    destination: &Path,
    branch: &str,
    depth: u32,
) -> Result<()> {
    let depth = depth.to_string();
    let (status, _stdout, stderr) = run_git(
        [
            "clone",
            "--quiet",
            "--no-local",
            "--no-checkout",
            "--no-recurse-submodules",
            "--single-branch",
            "--branch",
            branch,
            "--depth",
            &depth,
            path_to_arg(mirror)?,
            path_to_arg(destination)?,
        ],
        false,
        true,
    )
    .await?;
    if status.success() {
        return Ok(());
    }
    bail!("git local clone failed: {}", bounded_stderr(&stderr));
}

const fn pagination_page_limit(maximum: usize) -> usize {
    maximum.div_ceil(PAGE_SIZE) + 1
}

const fn repository_cap_allows(discovered: usize, maximum: usize) -> bool {
    discovered <= maximum
}

fn bounded_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(&stderr[..stderr.len().min(1024)])
        .trim()
        .to_owned()
}

fn write_manifest(directory: &Path, manifest: &Manifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)?;
    fs::write(directory.join(MANIFEST_FILE), bytes).context("writing source manifest")
}

fn current_manifest_matches(root: &Path, desired: &Manifest) -> Result<bool> {
    let Some(current) = current_generation_path(root)? else {
        return Ok(false);
    };
    let manifest = read_manifest(&current)?;
    Ok(manifest_matches(&manifest, desired))
}

fn manifest_matches(current: &Manifest, desired: &Manifest) -> bool {
    current.organization == desired.organization && current.repositories == desired.repositories
}

fn replace_current_link(root: &Path, generation_id: &str) -> Result<()> {
    ensure!(is_generation_id(generation_id), "invalid generation ID");
    let current = root.join("current");
    let next = root.join(format!(".current-next-{}", Uuid::now_v7()));
    let target = Path::new("generations").join(generation_id);
    create_directory_symlink(&target, &next)?;
    #[cfg(unix)]
    fs::rename(&next, &current).context("atomically replacing current source snapshot")?;
    #[cfg(windows)]
    {
        if current.exists() || fs::symlink_metadata(&current).is_ok() {
            fs::remove_file(&current).context("removing previous current source snapshot link")?;
        }
        fs::rename(&next, &current).context("replacing current source snapshot link")?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_directory_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).context("creating current source snapshot link")
}

#[cfg(windows)]
fn create_directory_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::windows::fs::symlink_dir(target, link).context("creating current source snapshot link")
}

#[cfg(not(any(unix, windows)))]
fn create_directory_symlink(_target: &Path, _link: &Path) -> Result<()> {
    bail!("source snapshots require directory symlink support")
}

fn touch_readiness(root: &Path, generation_id: Option<&str>) -> Result<()> {
    let generation_id =
        generation_id.ok_or_else(|| anyhow!("current source snapshot is missing"))?;
    ensure!(
        is_generation_id(generation_id),
        "invalid current generation ID"
    );
    fs::write(root.join(READY_FILE), format!("{generation_id}\n"))
        .context("writing source readiness marker")
}

fn current_generation_id(root: &Path) -> Result<Option<String>> {
    let current = root.join("current");
    let target = match fs::read_link(current) {
        Ok(target) => target,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("reading current source snapshot link"),
    };
    let components: Vec<_> = target.components().collect();
    let Some(generation) = components
        .get(1)
        .and_then(|component| component.as_os_str().to_str())
    else {
        bail!("current source snapshot link has an invalid target");
    };
    ensure!(
        components.len() == 2
            && components[0].as_os_str() == "generations"
            && is_generation_id(generation),
        "current source snapshot link has an invalid target"
    );
    Ok(Some(generation.to_owned()))
}

fn current_generation_path(root: &Path) -> Result<Option<PathBuf>> {
    Ok(current_generation_id(root)?.map(|id| root.join("generations").join(id)))
}

fn read_manifest(directory: &Path) -> Result<Manifest> {
    let bytes = fs::read(directory.join(MANIFEST_FILE)).context("reading source manifest")?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "source manifest exceeds size limit"
    );
    let manifest: Manifest = serde_json::from_slice(&bytes).context("parsing source manifest")?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_manifest(manifest: &Manifest) -> Result<()> {
    ensure!(
        is_safe_owner(&manifest.organization),
        "manifest has unsafe organization"
    );
    ensure!(
        manifest.repositories.len() <= 500,
        "manifest has too many repositories"
    );
    let mut seen = BTreeSet::new();
    for repository in &manifest.repositories {
        ensure!(
            is_safe_repo_name(&repository.name),
            "manifest has unsafe repository name"
        );
        ensure!(
            is_safe_branch(&repository.default_branch),
            "manifest has unsafe default branch"
        );
        ensure!(is_commit(&repository.commit), "manifest has invalid commit");
        ensure!(
            seen.insert(&repository.name),
            "manifest has duplicate repository"
        );
    }
    Ok(())
}

fn healthcheck(config: &Config) -> Result<()> {
    let Some(generation_id) = current_generation_id(&config.root)? else {
        bail!("no current source snapshot");
    };
    let generation = config.root.join("generations").join(&generation_id);
    ensure!(
        fs::metadata(&generation)?.is_dir(),
        "current source snapshot is not a directory"
    );
    let manifest = read_manifest(&generation)?;
    ensure!(
        manifest.organization == config.organization,
        "current source snapshot belongs to a different organization"
    );
    ensure!(
        manifest
            .repositories
            .iter()
            .any(|repository| repository.name == PRIMARY_REPOSITORY),
        "current source snapshot is missing the primary repository"
    );
    let marker = config.root.join(READY_FILE);
    let ready_generation =
        fs::read_to_string(&marker).context("reading source readiness marker")?;
    ensure!(
        ready_marker_matches(&ready_generation, &generation_id),
        "readiness marker does not match current snapshot"
    );
    let age = SystemTime::now()
        .duration_since(fs::metadata(marker)?.modified()?)
        .unwrap_or(Duration::ZERO);
    let maximum_age = config
        .interval
        .saturating_mul(3)
        .max(Duration::from_secs(300));
    ensure!(age <= maximum_age, "source readiness marker is stale");
    Ok(())
}

fn ready_marker_matches(marker: &str, generation_id: &str) -> bool {
    is_generation_id(generation_id) && marker.trim() == generation_id
}

fn cleanup_generations(root: &Path, retention: Duration) -> Result<()> {
    let current = current_generation_id(root)?;
    let generations = root.join("generations");
    for entry in fs::read_dir(generations)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_generation_id(name) || current.as_deref() == Some(name) {
            continue;
        }
        let path = entry.path();
        if let Err(error) = read_manifest(&path) {
            warn!(path = %path.display(), error = %error, "not removing generation without a valid manifest");
            continue;
        }
        let retired_at = match read_retirement_marker(&path) {
            Ok(Some(retired_at)) => retired_at,
            Ok(None) => {
                mark_generation_retired_at(&path, unix_timestamp()?)?;
                continue;
            }
            Err(error) => {
                warn!(path = %path.display(), error = %error, "not removing generation without a valid retirement marker");
                continue;
            }
        };
        if generation_is_expired(retired_at, retention)? {
            fs::remove_dir_all(&path)
                .with_context(|| format!("removing expired generation {}", path.display()))?;
        }
    }
    Ok(())
}

fn mark_generation_retired(root: &Path, generation_id: &str) -> Result<()> {
    ensure!(
        is_generation_id(generation_id),
        "invalid retired generation ID"
    );
    mark_generation_retired_at(
        &root.join("generations").join(generation_id),
        unix_timestamp()?,
    )
}

fn mark_generation_retired_at(generation: &Path, retired_at: u64) -> Result<()> {
    let metadata = fs::symlink_metadata(generation)?;
    ensure!(
        metadata.file_type().is_dir(),
        "retired generation is not a directory"
    );
    fs::write(generation.join(RETIRED_FILE), format!("{retired_at}\n"))
        .context("writing source generation retirement marker")
}

fn read_retirement_marker(generation: &Path) -> Result<Option<u64>> {
    let marker = generation.join(RETIRED_FILE);
    let contents = match fs::read_to_string(marker) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("reading source generation retirement marker"),
    };
    let retired_at = contents
        .trim()
        .parse::<u64>()
        .context("parsing source generation retirement marker")?;
    Ok(Some(retired_at))
}

fn generation_is_expired(retired_at: u64, retention: Duration) -> Result<bool> {
    let now = unix_timestamp()?;
    Ok(now.saturating_sub(retired_at) > retention.as_secs())
}

fn unix_timestamp() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")
        .map(|duration| duration.as_secs())
}

fn path_to_arg(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8"))
}

fn is_safe_owner(value: &str) -> bool {
    value.len() <= 39
        && !value.is_empty()
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn is_safe_repo_name(value: &str) -> bool {
    value.len() <= 100
        && !value.is_empty()
        && value != "."
        && value != ".."
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn is_safe_branch(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with(['-', '/'])
        && !value.ends_with(['.', '/'])
        && !value.contains("..")
        && !value.contains("@{")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
}

fn is_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_generation_id(value: &str) -> bool {
    Uuid::parse_str(value).is_ok()
}

fn is_staging_name(value: &str) -> bool {
    value
        .strip_prefix(STAGING_PREFIX)
        .is_some_and(is_generation_id)
}

impl ProcessGroupGuard {
    const fn new(process_id: u32) -> Self {
        #[cfg(not(target_os = "linux"))]
        let _ = process_id;
        Self {
            #[cfg(target_os = "linux")]
            process_id,
            armed: true,
        }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }

    fn terminate(&mut self) {
        if !self.armed {
            return;
        }
        #[cfg(target_os = "linux")]
        if let Ok(raw_pid) = i32::try_from(self.process_id)
            && let Some(pid) = rustix::process::Pid::from_raw(raw_pid)
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(unix)]
async fn wait_for_shutdown() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).context("listening for SIGTERM")?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("waiting for Ctrl-C"),
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() -> Result<()> {
    tokio::signal::ctrl_c()
        .await
        .context("waiting for shutdown signal")
}

fn initialize_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_untrusted_components() {
        assert!(is_safe_owner("ShieldBattery"));
        assert!(!is_safe_owner("Shield/Battery"));
        assert!(is_safe_repo_name("shieldbattery.net"));
        assert!(!is_safe_repo_name("../escape"));
        assert!(is_safe_branch("release/2026.08"));
        assert!(!is_safe_branch("--upload-pack=evil"));
        assert!(!is_safe_branch("branch@{1}"));
    }

    #[test]
    fn manifest_comparison_ignores_timestamp_only() {
        let repositories = vec![ManifestRepository {
            name: "repo".to_owned(),
            default_branch: "main".to_owned(),
            commit: "a".repeat(40),
        }];
        let old = Manifest {
            organization: "ShieldBattery".to_owned(),
            generated_at: 1,
            repositories: repositories.clone(),
        };
        let new = Manifest {
            organization: "ShieldBattery".to_owned(),
            generated_at: 2,
            repositories,
        };
        assert!(manifest_matches(&old, &new));
    }

    #[test]
    fn expiry_selection_is_strictly_after_retention() {
        assert!(
            !generation_is_expired(
                unix_timestamp().expect("clock"),
                Duration::from_secs(u64::MAX)
            )
            .expect("age")
        );
        assert!(generation_is_expired(0, Duration::ZERO).expect("age"));
    }

    #[test]
    fn manifest_validation_rejects_duplicates() {
        let repository = ManifestRepository {
            name: "repo".to_owned(),
            default_branch: "main".to_owned(),
            commit: "b".repeat(40),
        };
        let manifest = Manifest {
            organization: "ShieldBattery".to_owned(),
            generated_at: 1,
            repositories: vec![repository.clone(), repository],
        };
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn pagination_includes_one_page_to_prove_the_cap() {
        assert_eq!(pagination_page_limit(1), 2);
        assert_eq!(pagination_page_limit(100), 2);
        assert_eq!(pagination_page_limit(101), 3);
        assert!(repository_cap_allows(100, 100));
        assert!(!repository_cap_allows(101, 100));
    }

    #[test]
    fn readiness_marker_must_match_the_generation() {
        let generation = Uuid::now_v7().to_string();
        assert!(ready_marker_matches(
            &format!("{generation}\n"),
            &generation
        ));
        assert!(!ready_marker_matches("other", &generation));
        assert!(!ready_marker_matches(&generation, "not-a-uuid"));
    }

    #[test]
    fn staging_names_require_the_exact_prefix_and_a_uuid() {
        let id = Uuid::now_v7();
        assert!(is_staging_name(&format!("{STAGING_PREFIX}{id}")));
        assert!(!is_staging_name(&id.to_string()));
        assert!(!is_staging_name(".staging-not-a-uuid"));
        assert!(!is_staging_name(&format!("{STAGING_PREFIX}{id}-extra")));
    }

    #[test]
    fn mirror_cleanup_removes_only_obsolete_managed_directories() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let mirrors = temporary.path().join("mirrors");
        fs::create_dir(&mirrors).expect("mirrors directory");
        fs::create_dir(mirrors.join("current.git")).expect("current mirror");
        fs::create_dir(mirrors.join("obsolete.git")).expect("obsolete mirror");
        fs::create_dir(mirrors.join("operator-data")).expect("unmanaged directory");
        let manifest = Manifest {
            organization: "ShieldBattery".to_owned(),
            generated_at: 1,
            repositories: vec![ManifestRepository {
                name: "current".to_owned(),
                default_branch: "main".to_owned(),
                commit: "c".repeat(40),
            }],
        };

        cleanup_mirrors(temporary.path(), &manifest).expect("mirror cleanup");

        assert!(mirrors.join("current.git").is_dir());
        assert!(!mirrors.join("obsolete.git").exists());
        assert!(mirrors.join("operator-data").is_dir());
    }

    #[test]
    fn cleanup_retains_a_generation_until_its_retirement_period_elapses() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let root = temporary.path();
        let generations = root.join("generations");
        fs::create_dir(&generations).expect("generations directory");
        let generation_id = Uuid::now_v7().to_string();
        let generation = generations.join(&generation_id);
        fs::create_dir(&generation).expect("generation directory");
        write_manifest(
            &generation,
            &Manifest {
                organization: "ShieldBattery".to_owned(),
                generated_at: 0,
                repositories: Vec::new(),
            },
        )
        .expect("manifest");

        cleanup_generations(root, Duration::ZERO).expect("first cleanup");
        assert!(
            generation.is_dir(),
            "unretired generations must be retained"
        );
        assert!(
            read_retirement_marker(&generation)
                .expect("retirement marker")
                .is_some()
        );

        mark_generation_retired_at(&generation, 0).expect("old retirement marker");
        cleanup_generations(root, Duration::ZERO).expect("second cleanup");
        assert!(
            !generation.exists(),
            "expired retired generation must be removed"
        );
    }
}
