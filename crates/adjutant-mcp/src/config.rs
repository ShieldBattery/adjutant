use std::{env, fmt, net::SocketAddr, time::Duration};

use anyhow::{Context, Result, bail};

/// Runtime limits for the diagnostic database server.
#[derive(Clone)]
pub struct Config {
    /// Kept private so accidental debug logging cannot reveal credentials.
    pub(crate) database_url: String,
    pub bind: SocketAddr,
    pub max_connections: u32,
    pub max_rows: u32,
    pub max_response_bytes: usize,
    pub max_row_bytes: usize,
    pub max_http_request_bytes: usize,
    pub statement_timeout: Duration,
    pub lock_timeout: Duration,
    pub(crate) max_sql_bytes: usize,
    tailscale_hostname: Option<String>,
}

impl Config {
    pub const DEFAULT_MAX_ROWS: u32 = 100;
    pub const ABSOLUTE_MAX_ROWS: u32 = 500;
    pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1_048_576;
    pub const DEFAULT_MAX_ROW_BYTES: usize = 64 * 1024;
    pub const ABSOLUTE_MAX_ROW_BYTES: usize = 1_048_576;
    pub const DEFAULT_MAX_HTTP_REQUEST_BYTES: usize = 64 * 1024;
    pub const ABSOLUTE_MAX_HTTP_REQUEST_BYTES: usize = 128 * 1024;
    pub const DEFAULT_MAX_SQL_BYTES: usize = 16 * 1024;

    /// Loads configuration without ever rendering the database URL in an error.
    pub fn from_env() -> Result<Self> {
        let database_url = env::var("ADJUTANT_MCP_DATABASE_URL")
            .context("ADJUTANT_MCP_DATABASE_URL must be set")?;
        if database_url.trim().is_empty() {
            bail!("ADJUTANT_MCP_DATABASE_URL must not be empty");
        }

        let bind = required_or_default("ADJUTANT_MCP_BIND", "127.0.0.1:8081")
            .parse()
            .context("ADJUTANT_MCP_BIND must be a socket address")?;
        let max_connections = parse_env("ADJUTANT_MCP_MAX_CONNECTIONS", 4_u32)?;
        let max_rows = parse_env("ADJUTANT_MCP_MAX_ROWS", Self::DEFAULT_MAX_ROWS)?;
        let max_response_bytes = parse_env(
            "ADJUTANT_MCP_MAX_RESPONSE_BYTES",
            Self::DEFAULT_MAX_RESPONSE_BYTES,
        )?;
        let max_row_bytes = parse_env("ADJUTANT_MCP_MAX_ROW_BYTES", Self::DEFAULT_MAX_ROW_BYTES)?;
        let max_http_request_bytes = parse_env(
            "ADJUTANT_MCP_MAX_REQUEST_BYTES",
            Self::DEFAULT_MAX_HTTP_REQUEST_BYTES,
        )?;
        let statement_timeout_ms = parse_env("ADJUTANT_MCP_STATEMENT_TIMEOUT_MS", 5_000_u64)?;
        let lock_timeout_ms = parse_env("ADJUTANT_MCP_LOCK_TIMEOUT_MS", 1_000_u64)?;
        let tailscale_hostname = env::var("ADJUTANT_MCP_TAILSCALE_HOSTNAME")
            .ok()
            .filter(|value| !value.trim().is_empty());

        if max_connections == 0 {
            bail!("ADJUTANT_MCP_MAX_CONNECTIONS must be greater than zero");
        }
        if max_rows == 0 || max_rows > Self::ABSOLUTE_MAX_ROWS {
            bail!(
                "ADJUTANT_MCP_MAX_ROWS must be between 1 and {}",
                Self::ABSOLUTE_MAX_ROWS
            );
        }
        if !(1024..=16 * 1024 * 1024).contains(&max_response_bytes) {
            bail!("ADJUTANT_MCP_MAX_RESPONSE_BYTES must be between 1024 and 16777216");
        }
        if !(1024..=Self::ABSOLUTE_MAX_ROW_BYTES).contains(&max_row_bytes)
            || max_row_bytes > max_response_bytes
        {
            bail!(
                "ADJUTANT_MCP_MAX_ROW_BYTES must be between 1024 and min(1048576, ADJUTANT_MCP_MAX_RESPONSE_BYTES)"
            );
        }
        if !(Self::DEFAULT_MAX_SQL_BYTES..=Self::ABSOLUTE_MAX_HTTP_REQUEST_BYTES)
            .contains(&max_http_request_bytes)
        {
            bail!(
                "ADJUTANT_MCP_MAX_REQUEST_BYTES must be between {} and {}",
                Self::DEFAULT_MAX_SQL_BYTES,
                Self::ABSOLUTE_MAX_HTTP_REQUEST_BYTES
            );
        }
        if statement_timeout_ms == 0 || statement_timeout_ms > 30_000 {
            bail!("ADJUTANT_MCP_STATEMENT_TIMEOUT_MS must be between 1 and 30000");
        }
        if lock_timeout_ms == 0 || lock_timeout_ms > statement_timeout_ms {
            bail!(
                "ADJUTANT_MCP_LOCK_TIMEOUT_MS must be between 1 and ADJUTANT_MCP_STATEMENT_TIMEOUT_MS"
            );
        }
        if let Some(hostname) = &tailscale_hostname
            && (!hostname.ends_with(".ts.net")
                || hostname.contains('/')
                || hostname.contains(':')
                || hostname.chars().any(char::is_whitespace))
        {
            bail!("ADJUTANT_MCP_TAILSCALE_HOSTNAME must be a .ts.net hostname without a port");
        }

        Ok(Self {
            database_url,
            bind,
            max_connections,
            max_rows,
            max_response_bytes,
            max_row_bytes,
            max_http_request_bytes,
            statement_timeout: Duration::from_millis(statement_timeout_ms),
            lock_timeout: Duration::from_millis(lock_timeout_ms),
            max_sql_bytes: Self::DEFAULT_MAX_SQL_BYTES,
            tailscale_hostname,
        })
    }

    /// Hosts accepted by rmcp's DNS-rebinding check. Tailscale Serve forwards
    /// to the loopback listener, while local Codex uses the loopback authority.
    #[must_use]
    pub fn allowed_hosts(&self) -> Vec<String> {
        let port = self.bind.port();
        let mut hosts = vec![
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
            self.bind.to_string(),
        ];
        if let Some(hostname) = &self.tailscale_hostname {
            hosts.push(format!("{hostname}:8443"));
        }
        hosts
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Config")
            .field("database_url", &"<redacted>")
            .field("bind", &self.bind)
            .field("max_connections", &self.max_connections)
            .field("max_rows", &self.max_rows)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("max_row_bytes", &self.max_row_bytes)
            .field("max_http_request_bytes", &self.max_http_request_bytes)
            .field("statement_timeout", &self.statement_timeout)
            .field("lock_timeout", &self.lock_timeout)
            .field("max_sql_bytes", &self.max_sql_bytes)
            .field("tailscale_hostname", &self.tailscale_hostname)
            .finish()
    }
}

fn required_or_default(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn parse_env<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr + Copy,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match env::var(name) {
        Ok(value) => value
            .parse()
            .with_context(|| format!("{name} must be a valid number")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("could not read {name}")),
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use super::Config;

    #[test]
    fn row_cap_is_explicitly_bounded() {
        assert_eq!(Config::DEFAULT_MAX_ROWS, 100);
        assert_eq!(Config::ABSOLUTE_MAX_ROWS, 500);
    }

    #[test]
    fn debug_output_redacts_database_url() {
        let config = Config {
            database_url: "postgres://user:secret@example.test/diagnostics".to_owned(),
            bind: "127.0.0.1:8081".parse::<SocketAddr>().unwrap(),
            max_connections: 1,
            max_rows: 1,
            max_response_bytes: 1024,
            max_row_bytes: 1024,
            max_http_request_bytes: Config::DEFAULT_MAX_HTTP_REQUEST_BYTES,
            statement_timeout: Duration::from_secs(1),
            lock_timeout: Duration::from_secs(1),
            max_sql_bytes: Config::DEFAULT_MAX_SQL_BYTES,
            tailscale_hostname: None,
        };
        let rendered = format!("{config:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("secret"));
    }
}
