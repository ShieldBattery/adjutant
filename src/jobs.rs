use std::fmt;
use std::sync::Arc;

use serenity::all::{
    ChannelId, CreateAllowedMentions, CreateAttachment, CreateMessage, EditMessage, Http, MessageId,
};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;

use crate::codex::CodexRunner;
use crate::evidence::{EvidenceCollector, EvidenceRequest};
use crate::store::Store;

const INLINE_REPORT_CHARS: usize = 1_650;
const STATUS_ERROR: &str = "Diagnosis failed. Open the run inspector for details.";

pub struct DiagnosticJob {
    pub run_id: Uuid,
    pub title: String,
    pub request: EvidenceRequest,
    pub delivery: DiscordDelivery,
}

pub struct DiscordDelivery {
    pub http: Arc<Http>,
    pub output_channel: ChannelId,
    pub status_message: MessageId,
    pub source_url: String,
    pub run_url: Option<Url>,
}

#[derive(Clone)]
pub struct JobQueue {
    sender: mpsc::Sender<DiagnosticJob>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnqueueError {
    Full,
    Closed,
}

impl fmt::Display for EnqueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => formatter.write_str("the diagnostic queue is full"),
            Self::Closed => formatter.write_str("the diagnostic queue is shutting down"),
        }
    }
}

impl JobQueue {
    pub fn try_enqueue(&self, job: DiagnosticJob) -> Result<(), EnqueueError> {
        self.sender.try_send(job).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => EnqueueError::Full,
            mpsc::error::TrySendError::Closed(_) => EnqueueError::Closed,
        })
    }
}

#[must_use]
pub fn start(
    max_queued_jobs: usize,
    max_concurrent_jobs: usize,
    store: Store,
    collector: EvidenceCollector,
    runner: CodexRunner,
) -> (JobQueue, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(max_queued_jobs);
    let handle = tokio::spawn(run(receiver, max_concurrent_jobs, store, collector, runner));
    (JobQueue { sender }, handle)
}

async fn run(
    mut receiver: mpsc::Receiver<DiagnosticJob>,
    max_concurrent_jobs: usize,
    store: Store,
    collector: EvidenceCollector,
    runner: CodexRunner,
) {
    let permits = Arc::new(Semaphore::new(max_concurrent_jobs));
    let mut tasks = JoinSet::new();

    loop {
        let Ok(permit) = Arc::clone(&permits).acquire_owned().await else {
            break;
        };
        let Some(job) = receiver.recv().await else {
            drop(permit);
            break;
        };
        let job_store = store.clone();
        let job_collector = collector.clone();
        let job_runner = runner.clone();
        tasks.spawn(async move {
            let run_id = job.run_id;
            process(job, &job_store, &job_collector, &job_runner).await;
            drop(permit);
            run_id
        });

        while let Some(result) = tasks.try_join_next() {
            match result {
                Ok(run_id) => info!(%run_id, "diagnostic job finished"),
                Err(error) => error!(%error, "diagnostic job task panicked"),
            }
        }
    }

    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(run_id) => info!(%run_id, "diagnostic job finished during shutdown"),
            Err(error) => error!(%error, "diagnostic job task panicked during shutdown"),
        }
    }
}

async fn process(
    job: DiagnosticJob,
    store: &Store,
    collector: &EvidenceCollector,
    runner: &CodexRunner,
) {
    let run_id = job.run_id;
    info!(%run_id, title = %job.title, "starting diagnostic job");
    if let Err(error) = store.mark_running(run_id).await {
        fail_job(&job.delivery, store, run_id, &error).await;
        return;
    }
    update_status(
        &job.delivery,
        run_id,
        "🔎",
        "Collecting evidence and diagnosing…",
    )
    .await;

    let result = async {
        let workspace = collector.collect(run_id, &job.request).await?;
        store
            .set_evidence_manifest(run_id, &workspace.manifest_json)
            .await?;
        runner.run(run_id, &job.request.text, &workspace).await
    }
    .await;

    match result {
        Ok(report) => {
            if let Err(error) = store.complete_run(run_id, &report).await {
                fail_job(&job.delivery, store, run_id, &error).await;
                return;
            }
            update_status(&job.delivery, run_id, "✅", "Diagnosis complete.").await;
            if let Err(error) = send_report(&job.delivery, &job.title, &report).await {
                warn!(%run_id, %error, "diagnosis succeeded but Discord delivery failed");
            }
        }
        Err(error) => fail_job(&job.delivery, store, run_id, &error).await,
    }
}

async fn fail_job(delivery: &DiscordDelivery, store: &Store, run_id: Uuid, error: &anyhow::Error) {
    error!(%run_id, error = ?error, "diagnostic job failed");
    if let Err(store_error) = store.fail_run(run_id, &format!("{error:#}")).await {
        error!(%run_id, %store_error, "failed to persist diagnostic failure");
    }
    update_status(delivery, run_id, "❌", STATUS_ERROR).await;
}

async fn update_status(delivery: &DiscordDelivery, run_id: Uuid, marker: &str, status: &str) {
    let content = status_text(delivery, run_id, marker, status);
    let builder = EditMessage::new()
        .content(content)
        .allowed_mentions(CreateAllowedMentions::new());
    if let Err(error) = delivery
        .output_channel
        .edit_message(&delivery.http, delivery.status_message, builder)
        .await
    {
        warn!(%run_id, %error, "failed to update Discord status message");
    }
}

async fn send_report(delivery: &DiscordDelivery, title: &str, report: &str) -> anyhow::Result<()> {
    let heading = format!("## {}\n", truncate(title, 180));
    let builder = if report.chars().count() <= INLINE_REPORT_CHARS {
        CreateMessage::new().content(format!("{heading}{report}"))
    } else {
        let excerpt = truncate(report, INLINE_REPORT_CHARS);
        CreateMessage::new()
            .content(format!(
                "{heading}{excerpt}\n\n_The complete diagnosis is attached._"
            ))
            .add_file(CreateAttachment::bytes(report.as_bytes(), "diagnosis.md"))
    }
    .allowed_mentions(CreateAllowedMentions::new());
    delivery
        .output_channel
        .send_message(&delivery.http, builder)
        .await?;
    Ok(())
}

#[must_use]
pub fn status_text(delivery: &DiscordDelivery, run_id: Uuid, marker: &str, status: &str) -> String {
    let inspector = delivery
        .run_url
        .as_ref()
        .map_or_else(String::new, |url| format!(" · [inspect run]({url})"));
    format!(
        "{marker} **Adjutant** — {status}\nRun `{run_id}` · [source]({}){inspector}",
        delivery.source_url
    )
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut characters = value.chars();
    let mut truncated: String = characters.by_ref().take(max_chars).collect();
    if characters.next().is_some() {
        truncated.push('…');
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_preserves_unicode_boundaries() {
        assert_eq!(truncate("abc😀def", 4), "abc😀…");
        assert_eq!(truncate("short", 20), "short");
    }
}
