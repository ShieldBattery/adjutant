use std::collections::HashSet;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use url::Url;

// Keep the retired UI token protected in case an older deployment still supplies it.
const PROTECTED_CHILD_VARIABLES: &[&str] = &["ADJUTANT_UI_TOKEN", "DISCORD_TOKEN"];

#[derive(Clone)]
pub struct Config {
    pub discord_token: String,
    pub discord_guild_id: u64,
    pub discord_bug_report_channel_id: u64,
    pub discord_bug_report_webhook_id: u64,
    pub discord_request_channel_id: u64,
    pub discord_output_channel_id: u64,
    pub discord_mention_role_id: Option<u64>,
    pub discord_allowed_role_ids: HashSet<u64>,
    pub shieldbattery_public_url: Url,
    pub shieldbattery_internal_url: Option<Url>,
    pub codex_bin: String,
    pub codex_home: PathBuf,
    pub codex_profile: Option<String>,
    pub codex_model: Option<String>,
    pub shieldbattery_source_dir: PathBuf,
    pub codex_env_passthrough: Vec<String>,
    pub max_concurrent_jobs: usize,
    pub max_queued_jobs: usize,
    pub max_download_bytes: u64,
    pub max_game_artifacts: usize,
    pub max_game_artifact_bytes: u64,
    pub max_game_evidence_bytes: u64,
    pub max_archive_files: usize,
    pub max_expanded_bytes: u64,
    pub max_codex_events: usize,
    pub max_codex_event_bytes: usize,
    pub max_codex_event_line_bytes: usize,
    pub job_timeout: Duration,
    pub database_path: PathBuf,
    pub ui_bind: SocketAddr,
    pub ui_base_url: Option<Url>,
    pub run_retention_days: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let shieldbattery_internal_url = optional("SHIELDBATTERY_INTERNAL_URL")
            .map(|value| parse_internal_url(&value))
            .transpose()?;

        let ui_bind = validate_ui_bind(
            optional("ADJUTANT_UI_BIND")
                .unwrap_or_else(|| "127.0.0.1:8080".to_owned())
                .parse()
                .context("ADJUTANT_UI_BIND must be an IP:port socket address")?,
        )?;

        let codex_env_passthrough = comma_separated("CODEX_ENV_PASSTHROUGH");
        for name in &codex_env_passthrough {
            if !is_environment_name(name) {
                bail!("CODEX_ENV_PASSTHROUGH contains an invalid variable name: {name}");
            }
            if PROTECTED_CHILD_VARIABLES.contains(&name.as_str()) {
                bail!("{name} cannot be passed to Codex");
            }
        }

        let max_game_artifact_bytes =
            parse_positive_or("MAX_GAME_ARTIFACT_BYTES", 100 * 1024 * 1024)?;
        let max_game_evidence_bytes =
            parse_positive_or("MAX_GAME_EVIDENCE_BYTES", 256 * 1024 * 1024)?;
        if max_game_artifact_bytes > max_game_evidence_bytes {
            bail!("MAX_GAME_ARTIFACT_BYTES cannot exceed MAX_GAME_EVIDENCE_BYTES");
        }

        let discord_bug_report_channel_id = parse_required("DISCORD_BUG_REPORT_CHANNEL_ID")?;
        let discord_request_channel_id = parse_required("DISCORD_REQUEST_CHANNEL_ID")?;
        let discord_output_channel_id = parse_required("DISCORD_OUTPUT_CHANNEL_ID")?;
        validate_staff_channels(
            discord_bug_report_channel_id,
            discord_request_channel_id,
            discord_output_channel_id,
        )?;

        Ok(Self {
            discord_token: required("DISCORD_TOKEN")?,
            discord_guild_id: parse_required("DISCORD_GUILD_ID")?,
            discord_bug_report_channel_id,
            discord_bug_report_webhook_id: parse_required("DISCORD_BUG_REPORT_WEBHOOK_ID")?,
            discord_request_channel_id,
            discord_output_channel_id,
            discord_mention_role_id: parse_optional_positive_u64("DISCORD_MENTION_ROLE_ID")?,
            discord_allowed_role_ids: comma_separated("DISCORD_ALLOWED_ROLE_IDS")
                .into_iter()
                .map(|value| {
                    value
                        .parse::<u64>()
                        .with_context(|| format!("DISCORD_ALLOWED_ROLE_IDS contains {value:?}"))
                })
                .collect::<Result<_>>()?,
            shieldbattery_public_url: parse_public_url(&required("SHIELDBATTERY_PUBLIC_URL")?)?,
            shieldbattery_internal_url,
            codex_bin: optional("CODEX_BIN").unwrap_or_else(|| "codex".to_owned()),
            codex_home: optional("CODEX_HOME").map_or_else(default_codex_home, PathBuf::from),
            codex_profile: optional("CODEX_PROFILE"),
            codex_model: optional("CODEX_MODEL"),
            shieldbattery_source_dir: optional("SHIELDBATTERY_SOURCE_DIR")
                .map_or_else(|| PathBuf::from("../shieldbattery"), PathBuf::from),
            codex_env_passthrough,
            max_concurrent_jobs: parse_positive_or("MAX_CONCURRENT_JOBS", 2)?,
            max_queued_jobs: parse_positive_or("MAX_QUEUED_JOBS", 20)?,
            max_download_bytes: parse_positive_or("MAX_DOWNLOAD_BYTES", 32 * 1024 * 1024)?,
            max_game_artifacts: parse_positive_or("MAX_GAME_ARTIFACTS", 32)?,
            max_game_artifact_bytes,
            max_game_evidence_bytes,
            max_archive_files: parse_positive_or("MAX_ARCHIVE_FILES", 128)?,
            max_expanded_bytes: parse_positive_or("MAX_EXPANDED_BYTES", 256 * 1024 * 1024)?,
            max_codex_events: parse_positive_or("MAX_CODEX_EVENTS", 10_000)?,
            max_codex_event_bytes: parse_positive_or("MAX_CODEX_EVENT_BYTES", 32 * 1024 * 1024)?,
            max_codex_event_line_bytes: parse_positive_or(
                "MAX_CODEX_EVENT_LINE_BYTES",
                512 * 1024,
            )?,
            job_timeout: Duration::from_secs(parse_positive_or("JOB_TIMEOUT_SECONDS", 1800)?),
            database_path: optional("ADJUTANT_DATABASE_PATH")
                .map_or_else(|| PathBuf::from("data/adjutant.sqlite3"), PathBuf::from),
            ui_bind,
            ui_base_url: optional("ADJUTANT_UI_BASE_URL")
                .map(|value| parse_ui_url(&value))
                .transpose()?,
            run_retention_days: parse_positive_or("RUN_RETENTION_DAYS", 90)?,
        })
    }
}

pub(crate) fn validate_ui_bind(bind: SocketAddr) -> Result<SocketAddr> {
    if !bind.ip().is_loopback() {
        bail!("ADJUTANT_UI_BIND must be a loopback address; expose the UI through Tailscale Serve");
    }
    Ok(bind)
}

fn validate_staff_channels(alerts: u64, requests: u64, output: u64) -> Result<()> {
    if alerts == 0 || requests == 0 || output == 0 {
        bail!("Discord staff channel IDs must be nonzero");
    }
    if requests != output {
        bail!("DISCORD_OUTPUT_CHANNEL_ID must equal DISCORD_REQUEST_CHANNEL_ID (command-center)");
    }
    if alerts == requests {
        bail!("staff-alerts and command-center must be different channels");
    }
    Ok(())
}

fn required(name: &str) -> Result<String> {
    optional(name).with_context(|| format!("{name} must be set and non-empty"))
}

fn optional(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_required<T>(name: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    required(name)?
        .parse()
        .with_context(|| format!("{name} is invalid"))
}

fn parse_optional_positive_u64(name: &str) -> Result<Option<u64>> {
    parse_optional_positive_u64_value(name, optional(name))
}

fn parse_optional_positive_u64_value(name: &str, value: Option<String>) -> Result<Option<u64>> {
    value
        .map(|value| {
            let value = value
                .parse()
                .with_context(|| format!("{name} is invalid"))?;
            if value == 0 {
                bail!("{name} must be greater than zero");
            }
            Ok(value)
        })
        .transpose()
}

fn parse_positive_or<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr + PartialOrd + From<u8> + Copy,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let value = optional(name).map_or(Ok(default), |raw| {
        raw.parse().with_context(|| format!("{name} is invalid"))
    })?;
    if value <= T::from(0) {
        bail!("{name} must be greater than zero");
    }
    Ok(value)
}

fn comma_separated(name: &str) -> Vec<String> {
    optional(name)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_internal_url(value: &str) -> Result<Url> {
    let mut url = Url::parse(value).context("SHIELDBATTERY_INTERNAL_URL is not a valid URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("SHIELDBATTERY_INTERNAL_URL must be an HTTP(S) origin");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("SHIELDBATTERY_INTERNAL_URL cannot contain credentials, a query, or a fragment");
    }
    if url.path() != "/" {
        bail!("SHIELDBATTERY_INTERNAL_URL must not contain a path");
    }
    url.set_path("");
    Ok(url)
}

fn parse_public_url(value: &str) -> Result<Url> {
    let mut url = Url::parse(value).context("SHIELDBATTERY_PUBLIC_URL is not a valid URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("SHIELDBATTERY_PUBLIC_URL must be an HTTP(S) origin");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!(
            "SHIELDBATTERY_PUBLIC_URL cannot contain credentials, a path, a query, or a fragment"
        );
    }
    url.set_path("");
    Ok(url)
}

fn parse_ui_url(value: &str) -> Result<Url> {
    let mut url = Url::parse(value).context("ADJUTANT_UI_BASE_URL is not a valid URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("ADJUTANT_UI_BASE_URL must be an HTTP(S) URL");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("ADJUTANT_UI_BASE_URL cannot contain credentials, a query, or a fragment");
    }
    if url.path() != "/" {
        bail!("ADJUTANT_UI_BASE_URL must not contain a path");
    }
    url.set_path("");
    Ok(url)
}

fn default_codex_home() -> PathBuf {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
        .or_else(|| env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join(".codex")))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

fn is_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('A'..='Z' | '_'))
        && chars.all(|c| matches!(c, 'A'..='Z' | '0'..='9' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_listener_requires_loopback() {
        for address in ["127.0.0.1:8080", "127.0.0.2:8080", "[::1]:8080"] {
            let bind = address.parse().unwrap();
            assert_eq!(validate_ui_bind(bind).unwrap(), bind);
        }
        for address in [
            "0.0.0.0:8080",
            "[::]:8080",
            "192.0.2.1:8080",
            "192.168.1.1:8080",
            "100.64.0.1:8080",
            "[2001:db8::1]:8080",
        ] {
            assert!(validate_ui_bind(address.parse().unwrap()).is_err());
        }
    }

    #[test]
    fn enforces_exactly_two_nonzero_staff_channels() {
        assert!(super::validate_staff_channels(1, 2, 2).is_ok());
        for (alerts, requests, output) in [(1, 2, 3), (1, 1, 1), (0, 2, 2), (1, 0, 0)] {
            assert!(super::validate_staff_channels(alerts, requests, output).is_err());
        }
    }

    #[test]
    fn parses_optional_nonzero_mention_role_id() {
        assert_eq!(
            parse_optional_positive_u64_value("DISCORD_MENTION_ROLE_ID", None).unwrap(),
            None
        );
        assert_eq!(
            parse_optional_positive_u64_value(
                "DISCORD_MENTION_ROLE_ID",
                Some("123456789012345678".to_owned()),
            )
            .unwrap(),
            Some(123_456_789_012_345_678)
        );
        assert!(
            parse_optional_positive_u64_value("DISCORD_MENTION_ROLE_ID", Some("0".to_owned()))
                .is_err()
        );
        assert!(
            parse_optional_positive_u64_value(
                "DISCORD_MENTION_ROLE_ID",
                Some("not-a-role-id".to_owned()),
            )
            .is_err()
        );
    }

    #[test]
    fn validates_environment_names() {
        assert!(is_environment_name("DD_API_KEY"));
        assert!(is_environment_name("_PRIVATE_2"));
        assert!(!is_environment_name("lowercase"));
        assert!(!is_environment_name("2FAST"));
        assert!(!is_environment_name("HAS-DASH"));
    }

    #[test]
    fn accepts_only_internal_origins() {
        assert!(parse_internal_url("http://sb-prod").is_ok());
        assert!(parse_internal_url("https://sb-prod:8443/").is_ok());
        assert!(parse_internal_url("ftp://sb-prod").is_err());
        assert!(parse_internal_url("http://user:pass@sb-prod").is_err());
        assert!(parse_internal_url("http://sb-prod/internal").is_err());
    }

    #[test]
    fn accepts_only_shieldbattery_public_origins() {
        assert_eq!(
            parse_public_url("https://shieldbattery.net")
                .unwrap()
                .as_str(),
            "https://shieldbattery.net/"
        );
        assert!(parse_public_url("https://shieldbattery.net/admin").is_err());
    }

    #[test]
    fn accepts_only_inspection_origins() {
        assert_eq!(
            parse_ui_url("https://adjutant.example.ts.net")
                .unwrap()
                .as_str(),
            "https://adjutant.example.ts.net/"
        );
        assert!(parse_ui_url("https://user@adjutant.example.ts.net").is_err());
        assert!(parse_ui_url("https://adjutant.example.ts.net/private").is_err());
        assert!(parse_ui_url("file:///tmp/adjutant").is_err());
    }
}
