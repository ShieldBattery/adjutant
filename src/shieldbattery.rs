use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{Client, Response, redirect::Policy};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

#[derive(Clone)]
pub struct ShieldBatteryClient {
    http: Client,
    base_url: Url,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BugReport {
    pub id: Uuid,
    pub submitter_id: Option<i64>,
    pub details: String,
    pub logs_deleted: bool,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
    pub resolver_id: Option<i64>,
}

#[derive(Deserialize)]
struct BugReportResponse {
    report: BugReport,
}

impl ShieldBatteryClient {
    pub fn new(base_url: Url) -> Result<Self> {
        let http = Client::builder()
            .redirect(Policy::none())
            // Keep the request on the direct Tailnet path instead of a system-configured proxy.
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .user_agent(concat!("adjutant/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to create ShieldBattery HTTP client")?;
        Ok(Self { http, base_url })
    }

    pub async fn get_report(&self, report_id: Uuid) -> Result<BugReport> {
        let url = self.endpoint(report_id, false);
        let response = self
            .http
            .get(url)
            .send()
            .await
            .context("failed to request ShieldBattery bug report metadata")?;
        if !response.status().is_success() {
            bail!(
                "ShieldBattery bug report metadata request returned {}",
                response.status()
            );
        }
        let payload: BugReportResponse = response
            .json()
            .await
            .context("ShieldBattery returned invalid bug report metadata")?;
        if payload.report.id != report_id {
            bail!("ShieldBattery returned metadata for a different bug report");
        }
        Ok(payload.report)
    }

    pub async fn get_logs(&self, report_id: Uuid) -> Result<Response> {
        let response = self
            .http
            .get(self.endpoint(report_id, true))
            .send()
            .await
            .context("failed to request ShieldBattery bug report logs")?;
        if !response.status().is_success() {
            bail!(
                "ShieldBattery bug report logs request returned {}",
                response.status()
            );
        }
        Ok(response)
    }

    fn endpoint(&self, report_id: Uuid, logs: bool) -> Url {
        let mut url = self.base_url.clone();
        let suffix = if logs { "/logs" } else { "" };
        url.set_path(&format!("/internal/bug-reports/{report_id}{suffix}"));
        url
    }
}
