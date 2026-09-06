use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use serenity::all::{
    ChannelId, CreateAllowedMentions, CreateAttachment, CreateMessage, EditMessage, Http, MessageId,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;

use crate::codex::CodexRunner;
use crate::evidence::{EvidenceCollector, EvidenceRequest};
use crate::store::Store;

const INLINE_REPORT_CHARS: usize = 1_650;
const MAX_PROGRESS_POSTS: usize = 3;
const PROGRESS_POST_INTERVAL: Duration = Duration::from_secs(120);
const STATUS_ERROR: &str = "couldn't complete the diagnosis. check the run inspector for details.";

pub struct DiagnosticJob {
    pub run_id: Uuid,
    pub conversation_id: Uuid,
    pub title: String,
    pub request: EvidenceRequest,
    pub delivery: DiscordDelivery,
}

#[derive(Clone)]
pub struct DiscordDelivery {
    pub http: Arc<Http>,
    pub output_channel: ChannelId,
    pub status_message: MessageId,
    pub reply_to: MessageId,
    pub guild_id: u64,
    pub source_url: String,
    pub run_url: Option<Url>,
}

#[derive(Clone)]
pub struct JobQueue {
    sender: mpsc::Sender<QueuedJob>,
    capacity: Arc<Semaphore>,
}

struct QueuedJob {
    job: DiagnosticJob,
    reservation: OwnedSemaphorePermit,
}

pub struct JobShutdown {
    sender: oneshot::Sender<()>,
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
        let reservation = Arc::clone(&self.capacity)
            .try_acquire_owned()
            .map_err(|_| EnqueueError::Full)?;
        self.sender
            .try_send(QueuedJob { job, reservation })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => EnqueueError::Full,
                mpsc::error::TrySendError::Closed(_) => EnqueueError::Closed,
            })
    }
}

impl JobShutdown {
    pub fn shutdown(self) {
        let _ = self.sender.send(());
    }
}

#[must_use]
pub fn start(
    max_queued_jobs: usize,
    max_concurrent_jobs: usize,
    job_timeout: Duration,
    store: Store,
    collector: EvidenceCollector,
    runner: CodexRunner,
) -> (JobQueue, JobShutdown, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(max_queued_jobs);
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let handle = tokio::spawn(run(
        receiver,
        shutdown_receiver,
        max_concurrent_jobs,
        job_timeout,
        store,
        collector,
        runner,
    ));
    (
        JobQueue {
            sender,
            capacity: Arc::new(Semaphore::new(max_queued_jobs)),
        },
        JobShutdown {
            sender: shutdown_sender,
        },
        handle,
    )
}

async fn run(
    mut receiver: mpsc::Receiver<QueuedJob>,
    mut shutdown: oneshot::Receiver<()>,
    max_concurrent_jobs: usize,
    job_timeout: Duration,
    store: Store,
    collector: EvidenceCollector,
    runner: CodexRunner,
) {
    let mut pending = VecDeque::<QueuedJob>::new();
    let mut active = HashSet::new();
    let mut tasks = JoinSet::new();
    let mut accepting = true;
    loop {
        while tasks.len() < max_concurrent_jobs {
            let Some(queued) = take_ready(&mut pending, &active) else {
                break;
            };
            let job = queued.job;
            drop(queued.reservation);
            let conversation_id = job.conversation_id;
            active.insert(conversation_id);
            let job_store = store.clone();
            let job_collector = collector.clone();
            let job_runner = runner.clone();
            tasks.spawn(async move {
                use futures_util::FutureExt as _;
                let run_id = job.run_id;
                let delivery = job.delivery.clone();
                if std::panic::AssertUnwindSafe(process(
                    job,
                    &job_store,
                    &job_collector,
                    &job_runner,
                    job_timeout,
                ))
                .catch_unwind()
                .await
                .is_err()
                {
                    error!(%run_id, "diagnostic job panicked");
                    let panic_error = anyhow::anyhow!("diagnostic job stopped unexpectedly");
                    fail_job(&delivery, &job_store, run_id, conversation_id, &panic_error).await;
                }
                conversation_id
            });
        }
        if !accepting && pending.is_empty() && tasks.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            _ = &mut shutdown, if accepting => {
                accepting = false;
                receiver.close();
                while let Ok(job) = receiver.try_recv() { pending.push_back(job); }
                for queued in pending.drain(..) { cancel_job(queued.job, &store).await; }
            }
            finished = tasks.join_next(), if !tasks.is_empty() => {
                match finished {
                    Some(Ok(conversation)) => { active.remove(&conversation); }
                    Some(Err(error)) => error!(%error, "job scheduler task failed"),
                    None => {},
                }
            }
            next = receiver.recv(), if accepting => {
                if let Some(job) = next { pending.push_back(job); } else { accepting = false; }
            }
        }
    }
}

fn take_ready(pending: &mut VecDeque<QueuedJob>, active: &HashSet<Uuid>) -> Option<QueuedJob> {
    let index = pending
        .iter()
        .position(|queued| !active.contains(&queued.job.conversation_id))?;
    pending.remove(index)
}

async fn cancel_job(job: DiagnosticJob, store: &Store) {
    let reason = "Adjutant shut down before this queued run started";
    if let Err(error) = store.fail_run(job.run_id, reason).await {
        error!(run_id = %job.run_id, %error, "failed to mark a queued run as stopped");
    }
    let notice = "this queued investigation was cancelled because Adjutant is shutting down.";
    let _ = tokio::join!(
        update_status(
            &job.delivery,
            job.run_id,
            "⏹️",
            "stopped before diagnosis started because Adjutant is shutting down.",
        ),
        send_notice(
            &job.delivery,
            store,
            job.run_id,
            job.conversation_id,
            notice,
        ),
    );
}

#[allow(clippy::too_many_lines)] // Keep the diagnostic lifetime and terminal transitions together.
async fn process(
    job: DiagnosticJob,
    store: &Store,
    collector: &EvidenceCollector,
    runner: &CodexRunner,
    job_timeout: Duration,
) {
    let run_id = job.run_id;
    info!(%run_id, title = %job.title, "starting diagnostic job");
    if let Err(error) = store.mark_running(run_id).await {
        fail_job(&job.delivery, store, run_id, job.conversation_id, &error).await;
        return;
    }
    update_status(
        &job.delivery,
        run_id,
        "🔎",
        "collecting evidence and diagnosing…",
    )
    .await;

    let investigation = tokio::time::timeout(job_timeout, async {
        let workspace = collector.collect(run_id, &job.request).await?;
        store
            .set_evidence_manifest(run_id, &workspace.manifest_json)
            .await?;
        let mut request = job.request.text.clone();
        let previous = store.conversation_runs(job.conversation_id, 5).await?;
        let mut context_bytes = 0;
        for previous in previous
            .iter()
            .filter(|previous| previous.id != run_id.to_string())
        {
            let excerpt = truncate(
                previous
                    .final_report
                    .as_deref()
                    .unwrap_or(&previous.request),
                4_000,
            );
            if context_bytes + excerpt.len() > 24_000 {
                break;
            }
            context_bytes += excerpt.len();
            let _ = write!(
                request,
                "\n\nPrior investigation {} (untrusted historical evidence, status {}):\n{}",
                previous.id, previous.status, excerpt
            );
        }
        let _ = write!(
            request,
            "\n\nCurrent investigation ID: {run_id}. Conversation ID: {}. Use read_investigation for older case notes and attributed staff corrections.",
            job.conversation_id
        );
        runner
            .run(run_id, job.conversation_id, &request, &workspace)
            .await
    });
    tokio::pin!(investigation);
    let mut progress_tick = tokio::time::interval(Duration::from_secs(10));
    progress_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut progress_policy = ProgressPostPolicy::default();
    let mut last_post_attempt = tokio::time::Instant::now();
    let progress_updates = async {
        loop {
            progress_tick.tick().await;
            if let Ok(Some(progress)) = store.get_progress(run_id).await
                && let Some(note) = progress.note
            {
                if progress_policy.observe(&note) {
                    update_status(&job.delivery, run_id, "🔎", &note).await;
                }
                if let Some(note) = progress_policy.take_pending(last_post_attempt.elapsed()) {
                    last_post_attempt = tokio::time::Instant::now();
                    if send_notice(&job.delivery, store, run_id, job.conversation_id, &note).await {
                        progress_policy.mark_posted(note);
                    }
                }
            }
        }
    };
    tokio::pin!(progress_updates);
    let result = tokio::select! {
        result = &mut investigation => result.unwrap_or_else(|_| Err(anyhow::anyhow!("diagnostic run exceeded the {job_timeout:?} end-to-end timeout"))),
        () = &mut progress_updates => unreachable!("progress loop only ends on cancellation"),
    };

    match result {
        Ok(report) => {
            if let Err(error) = store.complete_run(run_id, &report).await {
                fail_job(&job.delivery, store, run_id, job.conversation_id, &error).await;
                return;
            }
            if let Err(error) = store
                .save_case(
                    run_id,
                    job.conversation_id,
                    &job.title,
                    &report,
                    &job.delivery.source_url,
                )
                .await
            {
                warn!(%run_id, %error, "could not save investigation memory");
            }
            update_status(&job.delivery, run_id, "✅", "done. diagnosis complete.").await;
            match send_report(&job.delivery, &job.title, &report).await {
                Ok(message) => {
                    let _ = store
                        .link_message(
                            run_id,
                            job.conversation_id,
                            job.delivery.guild_id,
                            message.channel_id.get(),
                            message.id.get(),
                        )
                        .await;
                }
                Err(error) => {
                    warn!(%run_id, %error, "diagnosis succeeded but Discord delivery failed");
                    let notice =
                        "couldn't post the diagnosis here. check the run inspector for the result.";
                    let _ = tokio::join!(
                        update_status(
                            &job.delivery,
                            run_id,
                            "⚠️",
                            "diagnosis complete, but posting the report failed. check the run inspector for the result.",
                        ),
                        send_notice(&job.delivery, store, run_id, job.conversation_id, notice,),
                    );
                }
            }
        }
        Err(error) => fail_job(&job.delivery, store, run_id, job.conversation_id, &error).await,
    }
}

async fn fail_job(
    delivery: &DiscordDelivery,
    store: &Store,
    run_id: Uuid,
    conversation_id: Uuid,
    error: &anyhow::Error,
) {
    error!(%run_id, error = ?error, "diagnostic job failed");
    if let Err(store_error) = store.fail_run(run_id, &format!("{error:#}")).await {
        error!(%run_id, %store_error, "failed to persist diagnostic failure");
    }
    let _ = tokio::join!(
        update_status(delivery, run_id, "❌", STATUS_ERROR),
        send_notice(delivery, store, run_id, conversation_id, STATUS_ERROR),
    );
}

pub async fn update_status(delivery: &DiscordDelivery, run_id: Uuid, marker: &str, status: &str) {
    let content = status_text(delivery, run_id, marker, status);
    let builder = EditMessage::new()
        .content(content)
        .allowed_mentions(CreateAllowedMentions::new().replied_user(false));
    if let Err(error) = tokio::time::timeout(Duration::from_secs(10), async {
        delivery
            .output_channel
            .edit_message(&delivery.http, delivery.status_message, builder)
            .await?;
        anyhow::Ok(())
    })
    .await
    .unwrap_or_else(|_| Err(anyhow::anyhow!("Discord status update timed out")))
    {
        warn!(%run_id, %error, "failed to update Discord status message");
    }
}

pub async fn send_notice(
    delivery: &DiscordDelivery,
    store: &Store,
    run_id: Uuid,
    conversation_id: Uuid,
    notice: &str,
) -> bool {
    let content = delivery.run_url.as_ref().map_or_else(
        || notice.to_owned(),
        |url| format!("{notice}\n[inspect run]({url})"),
    );
    let builder = CreateMessage::new()
        .content(truncate(&content, 1_200))
        .reference_message((delivery.output_channel, delivery.reply_to))
        .allowed_mentions(CreateAllowedMentions::new().replied_user(false));
    let message = match tokio::time::timeout(
        Duration::from_secs(10),
        delivery
            .output_channel
            .send_message(&delivery.http, builder),
    )
    .await
    {
        Ok(Ok(message)) => message,
        Ok(Err(error)) => {
            warn!(%run_id, %error, "failed to post Discord notice");
            return false;
        }
        Err(_) => {
            warn!(%run_id, "Discord notice timed out");
            return false;
        }
    };
    if let Err(error) = store
        .link_message(
            run_id,
            conversation_id,
            delivery.guild_id,
            message.channel_id.get(),
            message.id.get(),
        )
        .await
    {
        warn!(%run_id, %error, "failed to link Discord notice");
    }
    true
}

async fn send_report(
    delivery: &DiscordDelivery,
    title: &str,
    report: &str,
) -> anyhow::Result<serenity::all::Message> {
    let heading = format!("## {}\n", truncate(title, 180));
    let builder = if report.chars().count() <= INLINE_REPORT_CHARS {
        CreateMessage::new().content(format!("{heading}{report}"))
    } else {
        let excerpt = truncate(report, INLINE_REPORT_CHARS);
        CreateMessage::new()
            .content(format!(
                "{heading}{excerpt}\n\n_the complete diagnosis is attached._"
            ))
            .add_file(CreateAttachment::bytes(report.as_bytes(), "diagnosis.md"))
    }
    .allowed_mentions(CreateAllowedMentions::new().replied_user(false))
    .reference_message((delivery.output_channel, delivery.reply_to));
    Ok(tokio::time::timeout(
        Duration::from_secs(10),
        delivery
            .output_channel
            .send_message(&delivery.http, builder),
    )
    .await??)
}

#[derive(Default)]
struct ProgressPostPolicy {
    last_status_note: Option<String>,
    pending_note: Option<String>,
    last_posted_note: Option<String>,
    attempts: usize,
}

impl ProgressPostPolicy {
    fn observe(&mut self, note: &str) -> bool {
        if self.last_status_note.as_deref() == Some(note) {
            return false;
        }
        self.last_status_note = Some(note.to_owned());
        self.pending_note = Some(note.to_owned());
        true
    }

    fn take_pending(&mut self, elapsed: Duration) -> Option<String> {
        if elapsed < PROGRESS_POST_INTERVAL || self.attempts >= MAX_PROGRESS_POSTS {
            return None;
        }
        let note = self.pending_note.clone()?;
        if self.last_posted_note.as_deref() == Some(note.as_str()) {
            self.pending_note = None;
            return None;
        }
        // A timed-out POST may already have reached Discord. Cap attempts as well as confirmed
        // posts so retrying an uncertain delivery cannot exceed the progress-message allowance.
        self.attempts += 1;
        Some(note)
    }

    fn mark_posted(&mut self, note: String) {
        if self.pending_note.as_deref() == Some(note.as_str()) {
            self.pending_note = None;
        }
        self.last_posted_note = Some(note);
    }
}

#[must_use]
pub fn status_text(delivery: &DiscordDelivery, run_id: Uuid, marker: &str, status: &str) -> String {
    let inspector = delivery
        .run_url
        .as_ref()
        .map_or_else(String::new, |url| format!(" · [inspect run]({url})"));
    format!(
        "{marker} **Adjutant**: {status}\nrun `{run_id}` · [source]({}){inspector}",
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

    fn diagnostic(conversation_id: Uuid) -> DiagnosticJob {
        DiagnosticJob {
            run_id: Uuid::now_v7(),
            conversation_id,
            title: "test".to_owned(),
            request: EvidenceRequest {
                author: "staff".to_owned(),
                text: "question".to_owned(),
                bug_report_id: None,
                game_id: None,
                attachments: Vec::new(),
            },
            delivery: DiscordDelivery {
                http: Arc::new(serenity::all::Http::new("synthetic-test-token")),
                output_channel: ChannelId::new(2),
                status_message: MessageId::new(3),
                reply_to: MessageId::new(4),
                guild_id: 1,
                source_url: "https://discord.com/channels/1/2/4".to_owned(),
                run_url: None,
            },
        }
    }

    #[tokio::test]
    async fn queued_capacity_includes_jobs_taken_out_of_the_channel() {
        let (sender, mut receiver) = mpsc::channel(2);
        let queue = JobQueue {
            sender,
            capacity: Arc::new(Semaphore::new(2)),
        };
        queue.try_enqueue(diagnostic(Uuid::now_v7())).unwrap();
        queue.try_enqueue(diagnostic(Uuid::now_v7())).unwrap();
        let pending = receiver.recv().await.unwrap();
        assert_eq!(
            queue.try_enqueue(diagnostic(Uuid::now_v7())),
            Err(EnqueueError::Full)
        );
        drop(pending);
        queue.try_enqueue(diagnostic(Uuid::now_v7())).unwrap();
        receiver.close();
        drop(receiver);
        assert_eq!(
            queue.try_enqueue(diagnostic(Uuid::now_v7())),
            Err(EnqueueError::Closed)
        );
    }

    #[tokio::test]
    async fn followups_stay_ordered_without_blocking_other_conversations() {
        let first = Uuid::now_v7();
        let other = Uuid::now_v7();
        let permits = Arc::new(Semaphore::new(3));
        let mut pending = VecDeque::new();
        let mut ids = Vec::new();
        for conversation in [first, first, other] {
            let job = diagnostic(conversation);
            ids.push(job.run_id);
            pending.push_back(QueuedJob {
                job,
                reservation: Arc::clone(&permits).acquire_owned().await.unwrap(),
            });
        }
        let mut active = HashSet::from([first]);
        assert_eq!(
            take_ready(&mut pending, &active).unwrap().job.run_id,
            ids[2]
        );
        assert!(take_ready(&mut pending, &active).is_none());
        active.remove(&first);
        assert_eq!(
            take_ready(&mut pending, &active).unwrap().job.run_id,
            ids[0]
        );
        assert_eq!(
            take_ready(&mut pending, &active).unwrap().job.run_id,
            ids[1]
        );
    }

    #[test]
    fn pending_note_waits_for_the_initial_delay_and_survives_a_failed_send() {
        let mut policy = ProgressPostPolicy::default();
        assert!(policy.observe("checking logs"));
        assert_eq!(
            policy
                .take_pending(
                    PROGRESS_POST_INTERVAL
                        .checked_sub(Duration::from_secs(1))
                        .unwrap()
                )
                .as_deref(),
            None
        );
        assert_eq!(
            policy.take_pending(PROGRESS_POST_INTERVAL).as_deref(),
            Some("checking logs")
        );
        assert_eq!(
            policy.take_pending(PROGRESS_POST_INTERVAL).as_deref(),
            Some("checking logs")
        );
    }

    #[test]
    fn latest_progress_note_coalesces_earlier_pending_notes() {
        let mut policy = ProgressPostPolicy::default();
        assert!(policy.observe("checking logs"));
        assert!(policy.observe("checking the replay"));
        assert_eq!(
            policy.take_pending(PROGRESS_POST_INTERVAL).as_deref(),
            Some("checking the replay")
        );
    }

    #[test]
    fn previously_posted_note_is_not_posted_again() {
        let mut policy = ProgressPostPolicy::default();
        assert!(policy.observe("checking logs"));
        let note = policy.take_pending(PROGRESS_POST_INTERVAL).unwrap();
        policy.mark_posted(note);
        assert!(policy.observe("checking the replay"));
        assert!(policy.observe("checking logs"));
        assert_eq!(policy.take_pending(PROGRESS_POST_INTERVAL), None);
    }

    #[test]
    fn failed_progress_attempts_also_consume_the_message_allowance() {
        let mut policy = ProgressPostPolicy::default();
        policy.observe("checking logs");
        for _ in 0..MAX_PROGRESS_POSTS {
            assert!(policy.take_pending(PROGRESS_POST_INTERVAL).is_some());
        }
        assert!(policy.take_pending(PROGRESS_POST_INTERVAL).is_none());
    }

    #[test]
    fn standalone_progress_posts_stop_after_three() {
        let mut policy = ProgressPostPolicy::default();
        for note in ["one", "two", "three"] {
            assert!(policy.observe(note));
            let posted = policy.take_pending(PROGRESS_POST_INTERVAL).unwrap();
            policy.mark_posted(posted);
        }
        assert!(policy.observe("four"));
        assert_eq!(policy.take_pending(PROGRESS_POST_INTERVAL), None);
    }

    #[test]
    fn truncation_preserves_unicode_boundaries() {
        assert_eq!(truncate("abc😀def", 4), "abc😀…");
        assert_eq!(truncate("short", 20), "short");
    }
}
