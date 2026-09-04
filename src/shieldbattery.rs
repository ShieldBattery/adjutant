use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{Client, Response, redirect::Policy};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

const MAX_GAME_ARTIFACT_MANIFEST_BYTES: u64 = 256 * 1024;

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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameArtifacts {
    pub game_id: Uuid,
    pub flight_recordings: Vec<FlightRecordingArtifact>,
    pub replays: Vec<ReplayArtifact>,
    pub map: Option<MapArtifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlightRecordingArtifact {
    pub relay_id: i64,
    pub pinned: bool,
    /// Compressed size at rest. The downloaded JSON can be larger.
    pub size: u64,
    pub last_modified_ms: i64,
    pub download_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayArtifact {
    pub id: Uuid,
    pub uploader_user_id: i64,
    pub size: u64,
    pub sha256: String,
    pub frames: Option<u64>,
    pub download_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MapArtifact {
    pub id: Uuid,
    pub hash: String,
    pub format: MapFormat,
    pub name: String,
    pub download_path: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MapFormat {
    Scm,
    Scx,
}

impl MapFormat {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scm => "scm",
            Self::Scx => "scx",
        }
    }
}

impl ShieldBatteryClient {
    pub fn new(base_url: Url) -> Result<Self> {
        let http = Client::builder()
            .redirect(Policy::none())
            // Keep the request on the direct Tailnet path instead of a system-configured proxy.
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(180))
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

    pub async fn get_game_artifacts(&self, game_id: Uuid) -> Result<GameArtifacts> {
        let response = self
            .http
            .get(self.game_artifacts_endpoint(game_id))
            .send()
            .await
            .context("failed to request ShieldBattery game artifact metadata")?;
        if !response.status().is_success() {
            bail!(
                "ShieldBattery game artifact metadata request returned {}",
                response.status()
            );
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_GAME_ARTIFACT_MANIFEST_BYTES)
        {
            bail!(
                "ShieldBattery game artifact metadata exceeds the {MAX_GAME_ARTIFACT_MANIFEST_BYTES} byte limit"
            );
        }

        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("failed to read ShieldBattery game artifact metadata")?;
            let next_length = bytes
                .len()
                .checked_add(chunk.len())
                .context("ShieldBattery game artifact metadata size overflow")?;
            if u64::try_from(next_length)? > MAX_GAME_ARTIFACT_MANIFEST_BYTES {
                bail!(
                    "ShieldBattery game artifact metadata exceeds the {MAX_GAME_ARTIFACT_MANIFEST_BYTES} byte limit"
                );
            }
            bytes.extend_from_slice(&chunk);
        }

        let artifacts: GameArtifacts = serde_json::from_slice(&bytes)
            .context("ShieldBattery returned invalid game artifact metadata")?;
        validate_game_artifacts(game_id, &artifacts)?;
        Ok(artifacts)
    }

    pub async fn get_flight_recording(
        &self,
        game_id: Uuid,
        relay_id: i64,
    ) -> Result<Option<Response>> {
        self.get_game_artifact(format!(
            "/internal/games/{game_id}/artifacts/flight-recordings/{relay_id}"
        ))
        .await
    }

    pub async fn get_replay(&self, game_id: Uuid, replay_id: Uuid) -> Result<Option<Response>> {
        self.get_game_artifact(format!(
            "/internal/games/{game_id}/artifacts/replays/{replay_id}"
        ))
        .await
    }

    pub async fn get_map(&self, game_id: Uuid) -> Result<Option<Response>> {
        self.get_game_artifact(format!("/internal/games/{game_id}/artifacts/map"))
            .await
    }

    async fn get_game_artifact(&self, path: String) -> Result<Option<Response>> {
        let mut url = self.base_url.clone();
        url.set_path(&path);
        let response = self
            .http
            .get(url)
            .send()
            .await
            .context("failed to download a ShieldBattery game artifact")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            bail!(
                "ShieldBattery game artifact download returned {}",
                response.status()
            );
        }
        Ok(Some(response))
    }

    fn endpoint(&self, report_id: Uuid, logs: bool) -> Url {
        let mut url = self.base_url.clone();
        let suffix = if logs { "/logs" } else { "" };
        url.set_path(&format!("/internal/bug-reports/{report_id}{suffix}"));
        url
    }

    fn game_artifacts_endpoint(&self, game_id: Uuid) -> Url {
        let mut url = self.base_url.clone();
        url.set_path(&format!("/internal/games/{game_id}/artifacts"));
        url
    }
}

fn validate_game_artifacts(game_id: Uuid, artifacts: &GameArtifacts) -> Result<()> {
    if artifacts.game_id != game_id {
        bail!("ShieldBattery returned artifacts for a different game");
    }

    let base_path = format!("/internal/games/{game_id}/artifacts");
    let mut seen_flight_relays = HashSet::new();
    for artifact in &artifacts.flight_recordings {
        if !seen_flight_relays.insert(artifact.relay_id) {
            bail!(
                "ShieldBattery returned duplicate flight recording relay ID {}",
                artifact.relay_id
            );
        }
        let expected = format!("{base_path}/flight-recordings/{}", artifact.relay_id);
        if artifact.download_path != expected {
            bail!("ShieldBattery returned an invalid flight recording download path");
        }
    }

    let mut seen_replay_ids = HashSet::new();
    for artifact in &artifacts.replays {
        if !seen_replay_ids.insert(artifact.id) {
            bail!("ShieldBattery returned duplicate replay ID {}", artifact.id);
        }
        let expected = format!("{base_path}/replays/{}", artifact.id);
        if artifact.download_path != expected {
            bail!("ShieldBattery returned an invalid replay download path");
        }
        if !is_sha256_hex(&artifact.sha256) {
            bail!("ShieldBattery returned an invalid replay SHA-256 value");
        }
    }

    if let Some(artifact) = &artifacts.map {
        if artifact.download_path != format!("{base_path}/map") {
            bail!("ShieldBattery returned an invalid map download path");
        }
        if !is_sha256_hex(&artifact.hash) {
            bail!("ShieldBattery returned an invalid map hash");
        }
    }
    Ok(())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn valid_artifacts(game_id: Uuid) -> GameArtifacts {
        let replay_id = Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0bb").unwrap();
        GameArtifacts {
            game_id,
            flight_recordings: vec![FlightRecordingArtifact {
                relay_id: 7,
                pinned: true,
                size: 123,
                last_modified_ms: 1_700_000_000_000,
                download_path: format!("/internal/games/{game_id}/artifacts/flight-recordings/7"),
            }],
            replays: vec![ReplayArtifact {
                id: replay_id,
                uploader_user_id: 3,
                size: 456,
                sha256: "ab".repeat(32),
                frames: Some(12_345),
                download_path: format!("/internal/games/{game_id}/artifacts/replays/{replay_id}"),
            }],
            map: Some(MapArtifact {
                id: Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0bc").unwrap(),
                hash: "cd".repeat(32),
                format: MapFormat::Scx,
                name: "Fighting Spirit".to_owned(),
                download_path: format!("/internal/games/{game_id}/artifacts/map"),
            }),
        }
    }

    #[test]
    fn validates_game_binding_paths_hashes_and_uniqueness() {
        let game_id = Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0ba").unwrap();
        let artifacts = valid_artifacts(game_id);
        validate_game_artifacts(game_id, &artifacts).unwrap();

        let mut wrong_game = artifacts.clone();
        wrong_game.game_id = Uuid::nil();
        assert!(validate_game_artifacts(game_id, &wrong_game).is_err());

        let mut wrong_path = artifacts.clone();
        wrong_path.replays[0].download_path = format!("/internal/games/{game_id}/artifacts/map");
        assert!(validate_game_artifacts(game_id, &wrong_path).is_err());

        let mut wrong_hash = artifacts.clone();
        wrong_hash.map.as_mut().unwrap().hash = "CD".repeat(32);
        assert!(validate_game_artifacts(game_id, &wrong_hash).is_err());

        let mut duplicate = artifacts;
        duplicate
            .flight_recordings
            .push(duplicate.flight_recordings[0].clone());
        assert!(validate_game_artifacts(game_id, &duplicate).is_err());
    }

    #[test]
    fn deserializes_the_internal_api_contract() {
        let game_id = Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0ba").unwrap();
        let replay_id = Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0bb").unwrap();
        let map_id = Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0bc").unwrap();
        let value = json!({
            "gameId": game_id,
            "flightRecordings": [{
                "relayId": 7,
                "pinned": false,
                "size": 12,
                "lastModifiedMs": 1_700_000_000_000_i64,
                "downloadPath": format!(
                    "/internal/games/{game_id}/artifacts/flight-recordings/7"
                ),
            }],
            "replays": [{
                "id": replay_id,
                "uploaderUserId": 4,
                "size": 34,
                "sha256": "ab".repeat(32),
                "frames": null,
                "downloadPath": format!(
                    "/internal/games/{game_id}/artifacts/replays/{replay_id}"
                ),
            }],
            "map": {
                "id": map_id,
                "hash": "cd".repeat(32),
                "format": "scm",
                "name": "Python",
                "downloadPath": format!("/internal/games/{game_id}/artifacts/map"),
            },
        });

        let artifacts: GameArtifacts = serde_json::from_value(value).unwrap();
        validate_game_artifacts(game_id, &artifacts).unwrap();
        assert!(matches!(artifacts.map.unwrap().format, MapFormat::Scm));
    }
}
