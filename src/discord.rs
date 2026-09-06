use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::json;
use serenity::all::{
    ChannelId, Context, CreateAllowedMentions, CreateMessage, EventHandler, GatewayIntents,
    GuildId, Message, MessageId, MessageUpdateEvent, Ready,
};
use tokio::sync::Semaphore;
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;

use crate::codex::{CodexRunner, ConversationAction, SteerOutcome, SteeringUpdate};
use crate::config::Config;
use crate::evidence::{Attachment, EvidenceRequest};
use crate::jobs::{DiagnosticJob, DiscordDelivery, JobQueue, send_notice, update_status};
use crate::store::{NewRun, RunKind, RunLink, StaffMessage, Store};

const BUG_REPORT_PATH: &str = "/admin/bug-reports/";
const MAX_CONVERSATIONS: usize = 2;

pub struct DiscordHandler {
    config: Arc<Config>,
    store: Store,
    queue: JobQueue,
    runner: CodexRunner,
    bot_id: AtomicU64,
    conversations: Arc<Semaphore>,
    shutdown: tokio_util::sync::CancellationToken,
}

impl DiscordHandler {
    #[must_use]
    pub fn new(config: Arc<Config>, store: Store, queue: JobQueue, runner: CodexRunner) -> Self {
        Self {
            runner,
            config,
            store,
            queue,
            bot_id: AtomicU64::new(0),
            conversations: Arc::new(Semaphore::new(MAX_CONVERSATIONS)),
            shutdown: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub async fn shutdown_conversations(&self) {
        self.shutdown.cancel();
        self.conversations.close();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(65), async {
            while self.conversations.available_permits() < MAX_CONVERSATIONS {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
    }

    fn channel_allowed(&self, channel: u64) -> bool {
        channel == self.config.discord_bug_report_channel_id
            || channel == self.config.discord_request_channel_id
    }

    #[allow(clippy::too_many_lines)] // One ordered routing decision with early exits.
    async fn handle_message(&self, context: &Context, message: &Message) -> Result<()> {
        if self.shutdown.is_cancelled()
            || message.guild_id.map(GuildId::get) != Some(self.config.discord_guild_id)
            || !self.channel_allowed(message.channel_id.get())
        {
            return Ok(());
        }
        self.cache_message(message).await?;
        let bug_alert = message.channel_id.get() == self.config.discord_bug_report_channel_id
            && message.webhook_id.map(serenity::all::WebhookId::get)
                == Some(self.config.discord_bug_report_webhook_id);
        if !bug_alert
            && (message.webhook_id.is_some()
                || message.author.bot
                || !self.staff_member_is_allowed(message))
        {
            return Ok(());
        }
        if !self
            .store
            .claim_message(
                self.config.discord_guild_id,
                message.channel_id.get(),
                message.id.get(),
            )
            .await?
        {
            return Ok(());
        }
        if bug_alert {
            let report = find_bug_report_id_in_alert(
                &message.content,
                &self.config.shieldbattery_public_url,
            );
            if report.is_some() || !message.attachments.is_empty() {
                self.submit(context, message, RunKind::BugReport, None, None)
                    .await?;
            }
            return Ok(());
        }

        let linked = self.reply_link(message).await?;
        let bot_id = self.bot_id.load(Ordering::Relaxed);
        let addressed = directly_addressed(message, bot_id, self.config.discord_mention_role_id)
            || linked.is_some();
        let question = without_mention(
            &message.content,
            bot_id,
            self.config.discord_mention_role_id,
        );
        if addressed && is_status_question(&question) {
            let response = self.status_response(linked.as_ref(), "").await?;
            let sent = self.respond(context, message, &response).await?;
            self.link_response(message, &sent, linked.as_ref()).await?;
            return Ok(());
        }
        let acknowledgement = if addressed {
            let acknowledgement = self
                .respond(context, message, "got your message, taking a look.")
                .await?;
            // The router may steer a newer run or queue a follow-up. Message-to-run links are
            // immutable, so attach this acknowledgement only after its destination is known.
            self.cache_message(&acknowledgement).await?;
            Some(acknowledgement)
        } else {
            None
        };
        let Ok(_permit) = Arc::clone(&self.conversations).try_acquire_owned() else {
            self.link_acknowledgement(message, acknowledgement.as_ref(), linked.as_ref())
                .await?;
            if addressed {
                let sent = self
                    .respond(
                        context,
                        message,
                        "i'm handling other requests right now. try again shortly; i haven't started an investigation for this message.",
                    )
                    .await?;
                self.link_response(message, &sent, linked.as_ref()).await?;
            }
            return Ok(());
        };
        // Pin the target before triage. A late result must never steer a replacement run.
        let steer_target = if can_steer_message(
            message,
            linked.as_ref(),
            &self.config.shieldbattery_public_url,
        ) {
            linked
                .as_ref()
                .and_then(|link| Uuid::parse_str(&link.conversation_id).ok())
                .and_then(|conversation| self.runner.steering_target(conversation))
        } else {
            None
        };
        let decision_context = self
            .conversation_context(message, linked.as_ref(), steer_target)
            .await?;
        let decision_result = tokio::select! {
            biased;
            () = self.shutdown.cancelled() => {
                self.link_acknowledgement(message, acknowledgement.as_ref(), linked.as_ref()).await?;
                if addressed
                    && let Ok(sent) = self.respond(
                        context,
                        message,
                        "Adjutant is shutting down; no investigation was started for this message.",
                    ).await
                {
                    let _ = self.link_response(message, &sent, linked.as_ref()).await;
                }
                return Ok(());
            }
            result = self.runner.converse(message.id.get(), &decision_context, addressed) => result,
        };
        let decision = match decision_result {
            Ok(decision) => decision,
            Err(error) => {
                warn!(message_id = message.id.get(), %error, "conversational routing failed");
                self.link_acknowledgement(message, acknowledgement.as_ref(), linked.as_ref())
                    .await?;
                if addressed {
                    let sent = self
                        .respond(
                            context,
                            message,
                            "i couldn't process that request. try again; no investigation was started.",
                        )
                        .await?;
                    self.link_response(message, &sent, linked.as_ref()).await?;
                }
                return Ok(());
            }
        };
        if !matches!(
            decision.action,
            ConversationAction::Investigate | ConversationAction::Steer
        ) {
            self.link_acknowledgement(message, acknowledgement.as_ref(), linked.as_ref())
                .await?;
        }
        if let Some(link) = &linked
            && matches!(decision.action, ConversationAction::Remember)
        {
            // Keep the original staff statement and source link, without promoting it to a fact.
            self.store
                .save_case_observation(
                    Uuid::parse_str(&link.conversation_id)?,
                    &message.link(),
                    &message.content,
                )
                .await?;
        }
        match decision.action {
            ConversationAction::Ignore => {
                if addressed {
                    let sent = self
                        .respond(context, message, "what would you like me to check?")
                        .await?;
                    self.link_response(message, &sent, linked.as_ref()).await?;
                }
            }
            ConversationAction::Investigate => {
                self.submit(
                    context,
                    message,
                    RunKind::StaffRequest,
                    linked.as_ref(),
                    acknowledgement.as_ref(),
                )
                .await?;
            }
            ConversationAction::Steer => {
                self.steer_or_follow_up(
                    context,
                    message,
                    linked.as_ref(),
                    acknowledgement.as_ref(),
                    steer_target,
                )
                .await?;
            }
            ConversationAction::Status => {
                let response = self
                    .status_response(linked.as_ref(), &decision.query)
                    .await?;
                let sent = self.respond(context, message, &response).await?;
                self.link_response(message, &sent, linked.as_ref()).await?;
            }
            ConversationAction::Remember | ConversationAction::Reply => {
                if matches!(decision.action, ConversationAction::Remember) && linked.is_none() {
                    let sent = self
                        .respond(
                            context,
                            message,
                            "please reply to the investigation this finding belongs to so i can keep the correction linked to its evidence.",
                        )
                        .await?;
                    self.link_response(message, &sent, linked.as_ref()).await?;
                    return Ok(());
                }
                let response = if decision.reply.trim().is_empty() {
                    "what would you like me to check?"
                } else {
                    &decision.reply
                };
                let sent = self.respond(context, message, response).await?;
                self.link_response(message, &sent, linked.as_ref()).await?;
            }
        }
        Ok(())
    }

    async fn steer_or_follow_up(
        &self,
        context: &Context,
        message: &Message,
        linked: Option<&RunLink>,
        acknowledgement: Option<&Message>,
        target: Option<Uuid>,
    ) -> Result<()> {
        let Some(link) = linked else {
            let sent = self
                .respond(
                    context,
                    message,
                    "reply to the investigation you want to update so i know where to add that.",
                )
                .await?;
            self.link_response(message, &sent, None).await?;
            return Ok(());
        };
        if let Some(run_id) = target
            .filter(|_| can_steer_message(message, linked, &self.config.shieldbattery_public_url))
        {
            let update = SteeringUpdate {
                guild_id: self.config.discord_guild_id,
                channel_id: message.channel_id.get(),
                message_id: message.id.get(),
                author_id: message.author.id.get(),
                author: message.author.name.clone(),
                text: message.content.clone(),
                source_url: message.link(),
            };
            let outcome = self.runner.steer(run_id, update).await;
            match outcome {
                SteerOutcome::Accepted | SteerOutcome::Uncertain => {
                    let target_link = RunLink {
                        run_id: run_id.to_string(),
                        conversation_id: link.conversation_id.clone(),
                    };
                    self.link_acknowledgement(message, acknowledgement, Some(&target_link))
                        .await?;
                    let response = if outcome == SteerOutcome::Accepted {
                        if let Err(error) = self
                            .store
                            .save_case_observation(
                                Uuid::parse_str(&link.conversation_id)?,
                                &message.link(),
                                &message.content,
                            )
                            .await
                        {
                            warn!(%run_id, %error, "could not save an accepted staff update as case context");
                        }
                        "added that to the investigation i'm running."
                    } else {
                        "i couldn't confirm that update reached the investigation. check the run inspector before sending it again."
                    };
                    let sent = self.respond(context, message, response).await?;
                    self.link_response(message, &sent, Some(&target_link))
                        .await?;
                    return Ok(());
                }
                SteerOutcome::Unavailable | SteerOutcome::Full | SteerOutcome::TooLarge => {
                    info!(%run_id, ?outcome, "queuing staff update as a follow-up investigation");
                }
            }
        }
        // The target finished, is saturated, or this update needs evidence collection. Retain the
        // original message and normal archive/queue limits through the existing follow-up path.
        self.submit(
            context,
            message,
            RunKind::StaffRequest,
            linked,
            acknowledgement,
        )
        .await
    }

    async fn reply_link(&self, message: &Message) -> Result<Option<RunLink>> {
        if let Some(reference) = &message.message_reference
            && self.channel_allowed(reference.channel_id.get())
            && reference
                .guild_id
                .is_none_or(|guild| guild.get() == self.config.discord_guild_id)
            && let Some(id) = reference.message_id
        {
            return self
                .store
                .message_run(
                    self.config.discord_guild_id,
                    reference.channel_id.get(),
                    id.get(),
                )
                .await;
        }
        Ok(None)
    }

    async fn cache_message(&self, message: &Message) -> Result<()> {
        self.store
            .store_staff_message(&StaffMessage {
                guild_id: self.config.discord_guild_id,
                channel_id: message.channel_id.get(),
                message_id: message.id.get(),
                author: format!("{} ({})", message.author.name, message.author.id),
                content: message.content.clone(),
                reply_to: message
                    .message_reference
                    .as_ref()
                    .and_then(|reference| reference.message_id.map(MessageId::get)),
                created_at_ms: message.timestamp.unix_timestamp().saturating_mul(1000),
            })
            .await
    }

    async fn conversation_context(
        &self,
        message: &Message,
        linked: Option<&RunLink>,
        steer_target: Option<Uuid>,
    ) -> Result<String> {
        let recent = self
            .store
            .recent_staff_messages(self.config.discord_guild_id, message.channel_id.get(), 12)
            .await?;
        let recent: Vec<_> = recent.into_iter().map(|entry| json!({"message_id":entry.message_id.to_string(),"author":entry.author,"content":truncate(&entry.content, 1000),"reply_to":entry.reply_to.map(|id| id.to_string())})).collect();
        let active = self.store.active_runs(8).await?;
        let active: Vec<_> = active
            .iter()
            .map(|run| json!({"run_id":run.id,"title":run.title,"status":run.status}))
            .collect();
        let reply = message.referenced_message.as_ref().filter(|reply| self.channel_allowed(reply.channel_id.get()))
            .map(|reply| json!({"author":reply.author.name,"message_id":reply.id.to_string(),"content":truncate(&reply.content,2000)}));
        bounded_conversation_context(json!({
            "current_message":{"guild_id":self.config.discord_guild_id.to_string(),"channel_id":message.channel_id.get().to_string(),"message_id":message.id.get().to_string(),"author":message.author.name,"content":truncate(&message.content,8000),"attachments":message.attachments.iter().take(20).map(|a| truncate(&a.filename,100)).collect::<Vec<_>>()},
            "recent_messages":recent,"referenced_message":reply,"linked_investigation":linked,
            "active_investigations":active,
            "steerable_investigation":steer_target.map(|id| json!({"run_id":id})),
            "staff_alerts_channel":self.config.discord_bug_report_channel_id.to_string(),
            "command_center_channel":self.config.discord_request_channel_id.to_string(),
        }))
    }

    async fn status_response(&self, linked: Option<&RunLink>, query: &str) -> Result<String> {
        let runs = if let Some(link) = linked {
            self.store
                .conversation_runs(Uuid::parse_str(&link.conversation_id)?, 3)
                .await?
        } else if let Ok(id) = Uuid::parse_str(query.trim()) {
            self.store
                .get_run(&id.to_string())
                .await?
                .into_iter()
                .collect()
        } else {
            self.store.active_runs(8).await?
        };
        if runs.is_empty() {
            if let Some(link) = linked {
                let cases = self
                    .store
                    .case_history(Uuid::parse_str(&link.conversation_id)?, 1)
                    .await?;
                if let Some(case) = cases.first() {
                    return Ok(format!(
                        "**{}**: historical investigation. the detailed run record has expired; the case note is retained.\n\n{}\n\n[original request]({})",
                        truncate(&case.title, 100),
                        truncate(&case.summary, 1_300),
                        case.source_url
                    ));
                }
            }
            return Ok("there are no matching active investigations. reply to an earlier investigation to ask about that one.".to_owned());
        }
        let mut parts = Vec::new();
        for run in runs {
            let mut part = format!("**{}**: {}", truncate(&run.title, 100), run.status);
            if let Some(progress) = self.store.get_progress(Uuid::parse_str(&run.id)?).await? {
                if let Some(note) = progress.note {
                    let _ = write!(part, "\nlast progress note: {}", truncate(&note, 450));
                    if let Some(updated) = progress.note_updated_at_ms {
                        let _ = write!(part, " (reported <t:{}:R>)", updated / 1000);
                    }
                }
                if run.status == "running"
                    && let Some(activity) = progress.activity
                {
                    let _ = write!(
                        part,
                        "\nlast observed activity: {}",
                        truncate(&activity, 150)
                    );
                    if let Some(updated) = progress.activity_updated_at_ms {
                        let _ = write!(part, " (<t:{}:R>)", updated / 1000);
                    }
                }
                let _ = write!(part, "\nupdated <t:{}:R>.", progress.updated_at_ms / 1000);
            }
            let _ = write!(
                part,
                "\n[request](https://discord.com/channels/{}/{}/{})",
                run.discord_guild_id, run.discord_channel_id, run.discord_message_id
            );
            if let Some(url) = self.run_url(Uuid::parse_str(&run.id)?) {
                let _ = write!(part, " · [inspect]({url})");
            }
            if parts.iter().map(String::len).sum::<usize>() + part.len() > 5_000 {
                break;
            }
            parts.push(part);
        }
        Ok(truncate(&parts.join("\n\n"), 1_850))
    }

    async fn respond(&self, context: &Context, source: &Message, text: &str) -> Result<Message> {
        Ok(tokio::time::timeout(
            std::time::Duration::from_secs(10),
            source.channel_id.send_message(
                &context.http,
                CreateMessage::new()
                    .content(truncate(text, 1_900))
                    .reference_message(source)
                    .allowed_mentions(CreateAllowedMentions::new().replied_user(false)),
            ),
        )
        .await??)
    }

    async fn link_acknowledgement(
        &self,
        source: &Message,
        acknowledgement: Option<&Message>,
        linked: Option<&RunLink>,
    ) -> Result<()> {
        if let Some(acknowledgement) = acknowledgement {
            self.link_response(source, acknowledgement, linked).await?;
        }
        Ok(())
    }

    async fn link_response(
        &self,
        source: &Message,
        response: &Message,
        linked: Option<&RunLink>,
    ) -> Result<()> {
        if let Some(link) = linked {
            let run = Uuid::parse_str(&link.run_id)?;
            let conversation = Uuid::parse_str(&link.conversation_id)?;
            for message in [source, response] {
                self.store
                    .link_message(
                        run,
                        conversation,
                        self.config.discord_guild_id,
                        message.channel_id.get(),
                        message.id.get(),
                    )
                    .await?;
            }
        }
        self.cache_message(response).await?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // Persist and acknowledge before enqueueing.
    async fn submit(
        &self,
        context: &Context,
        message: &Message,
        kind: RunKind,
        linked: Option<&RunLink>,
        acknowledgement: Option<&Message>,
    ) -> Result<()> {
        let mut bug_report_id = if matches!(kind, RunKind::BugReport) {
            find_bug_report_id_in_alert(&message.content, &self.config.shieldbattery_public_url)
        } else {
            find_bug_report_id_in_text(&message.content, &self.config.shieldbattery_public_url)
        };
        if bug_report_id.is_none()
            && let Some(link) = linked
            && let Some(previous) = self.store.get_run(&link.run_id).await?
        {
            bug_report_id = previous
                .bug_report_id
                .and_then(|id| Uuid::parse_str(&id).ok());
        }
        let title = title_for(
            kind,
            bug_report_id,
            &without_mention(
                &message.content,
                self.bot_id.load(Ordering::Relaxed),
                self.config.discord_mention_role_id,
            ),
        );
        let request_text = if message.content.trim().is_empty() {
            "Inspect the attached evidence and diagnose the reported problem.".to_owned()
        } else {
            message.content.trim().to_owned()
        };
        let evidence = EvidenceRequest {
            author: message.author.name.clone(),
            text: request_text.clone(),
            bug_report_id,
            game_id: find_game_id(&message.content, &self.config.shieldbattery_public_url),
            attachments: discord_attachments(message)?,
        };
        let run = NewRun::new(
            kind,
            title.clone(),
            request_text,
            bug_report_id,
            self.config.discord_guild_id,
            message.channel_id.get(),
            message.id.get(),
        );
        self.store.create_run(&run).await?;
        let conversation_id = linked
            .map(|link| Uuid::parse_str(&link.conversation_id))
            .transpose()?
            .unwrap_or(run.id);
        self.store
            .link_message(
                run.id,
                conversation_id,
                self.config.discord_guild_id,
                message.channel_id.get(),
                message.id.get(),
            )
            .await?;
        if let Some(acknowledgement) = acknowledgement {
            self.store
                .link_message(
                    run.id,
                    conversation_id,
                    self.config.discord_guild_id,
                    acknowledgement.channel_id.get(),
                    acknowledgement.id.get(),
                )
                .await?;
        }
        let source_url = message.link();
        let run_url = self.run_url(run.id);
        // Automatic alert results live in command-center; addressed human conversations reply in place.
        let output_channel = if matches!(kind, RunKind::BugReport) {
            ChannelId::new(self.config.discord_output_channel_id)
        } else {
            message.channel_id
        };
        let queued = initial_status(run.id, &source_url, run_url.as_ref());
        let status_result: Result<Message> = if output_channel == message.channel_id {
            self.respond(context, message, &queued).await
        } else {
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                output_channel.send_message(
                    &context.http,
                    CreateMessage::new()
                        .content(queued)
                        .allowed_mentions(CreateAllowedMentions::new().replied_user(false)),
                ),
            )
            .await
            {
                Ok(result) => result.map_err(Into::into),
                Err(error) => Err(error.into()),
            }
        };
        let status_message = match status_result {
            Ok(status) => status,
            Err(error) => {
                self.store
                    .fail_run(run.id, "Could not post the run status in Discord")
                    .await?;
                return Err(error);
            }
        };
        self.store
            .link_message(
                run.id,
                conversation_id,
                self.config.discord_guild_id,
                output_channel.get(),
                status_message.id.get(),
            )
            .await?;
        self.cache_message(&status_message).await?;
        let reply_to = if output_channel == message.channel_id {
            message.id
        } else {
            status_message.id
        };
        let delivery = DiscordDelivery {
            http: Arc::clone(&context.http),
            output_channel,
            status_message: status_message.id,
            reply_to,
            guild_id: self.config.discord_guild_id,
            source_url,
            run_url,
        };
        let job = DiagnosticJob {
            run_id: run.id,
            conversation_id,
            title,
            request: evidence,
            delivery: delivery.clone(),
        };
        if let Err(error) = self.queue.try_enqueue(job) {
            self.store.fail_run(run.id, &error.to_string()).await?;
            let notice =
                format!("couldn't queue this investigation: {error}. please try again later.");
            let _ = tokio::join!(
                update_status(&delivery, run.id, "❌", &notice),
                send_notice(&delivery, &self.store, run.id, conversation_id, &notice,),
            );
        }
        Ok(())
    }

    fn staff_member_is_allowed(&self, message: &Message) -> bool {
        self.config.discord_allowed_role_ids.is_empty()
            || message.member.as_ref().is_some_and(|member| {
                member
                    .roles
                    .iter()
                    .any(|role| self.config.discord_allowed_role_ids.contains(&role.get()))
            })
    }
    fn run_url(&self, run_id: Uuid) -> Option<Url> {
        self.config
            .ui_base_url
            .as_ref()
            .and_then(|base| base.join(&format!("runs/{run_id}")).ok())
    }
}

#[serenity::async_trait]
impl EventHandler for DiscordHandler {
    async fn message(&self, context: Context, message: Message) {
        if let Err(error) = self.handle_message(&context, &message).await {
            error!(message_id=message.id.get(),error=?error,"failed to handle Discord message");
        }
    }
    async fn ready(&self, _context: Context, ready: Ready) {
        self.bot_id.store(ready.user.id.get(), Ordering::Relaxed);
        info!(user=%ready.user.name,"Discord bot connected");
    }
    async fn message_delete(
        &self,
        _context: Context,
        channel: ChannelId,
        id: MessageId,
        guild: Option<GuildId>,
    ) {
        if guild.map(GuildId::get) == Some(self.config.discord_guild_id)
            && self.channel_allowed(channel.get())
        {
            let _ = self
                .store
                .delete_staff_message(self.config.discord_guild_id, channel.get(), id.get())
                .await;
        }
    }
    async fn message_delete_bulk(
        &self,
        context: Context,
        channel: ChannelId,
        ids: Vec<MessageId>,
        guild: Option<GuildId>,
    ) {
        for id in ids {
            self.message_delete(context.clone(), channel, id, guild)
                .await;
        }
    }
    async fn message_update(
        &self,
        _context: Context,
        _old: Option<Message>,
        new: Option<Message>,
        event: MessageUpdateEvent,
    ) {
        if !self.channel_allowed(event.channel_id.get())
            || event.guild_id.map(GuildId::get) != Some(self.config.discord_guild_id)
        {
            return;
        }
        // Drop stale cached text even when Discord sends only a partial update. History can refetch it.
        let _ = self
            .store
            .delete_staff_message(
                self.config.discord_guild_id,
                event.channel_id.get(),
                event.id.get(),
            )
            .await;
        if let Some(new) = new {
            let _ = self.cache_message(&new).await;
        }
    }
}

#[must_use]
pub const fn gateway_intents() -> GatewayIntents {
    GatewayIntents::GUILD_MESSAGES.union(GatewayIntents::MESSAGE_CONTENT)
}

fn bounded_conversation_context(mut context: serde_json::Value) -> Result<String> {
    // Keep structured service IDs intact when unusually large messages require trimming.
    loop {
        let encoded = serde_json::to_string(&context)?;
        if encoded.len() <= 40 * 1024 {
            return Ok(encoded);
        }
        if let Some(recent) = context["recent_messages"].as_array_mut()
            && recent.pop().is_some()
        {
            continue;
        }
        if let Some(content) = context["current_message"]["content"].as_str()
            && content.chars().count() > 1000
        {
            context["current_message"]["content"] = json!(truncate(content, 1000));
            continue;
        }
        anyhow::bail!("conversation context exceeded its byte budget");
    }
}

fn can_steer_message(message: &Message, linked: Option<&RunLink>, public_url: &Url) -> bool {
    linked.is_some()
        && message.attachments.is_empty()
        && !message.content.trim().is_empty()
        && find_game_id(&message.content, public_url).is_none()
        && find_bug_report_id_in_text(&message.content, public_url).is_none()
}

fn directly_addressed(message: &Message, bot_id: u64, mention_role_id: Option<u64>) -> bool {
    (bot_id != 0
        && (message.mentions.iter().any(|user| user.id.get() == bot_id)
            || message
                .referenced_message
                .as_ref()
                .is_some_and(|reply| reply.author.id.get() == bot_id)))
        || mention_role_id.is_some_and(|role_id| {
            message
                .mention_roles
                .iter()
                .any(|role| role.get() == role_id)
        })
}

fn without_mention(text: &str, bot_id: u64, mention_role_id: Option<u64>) -> String {
    let mut stripped = text
        .replace(&format!("<@{bot_id}>"), "")
        .replace(&format!("<@!{bot_id}>"), "");
    if let Some(role_id) = mention_role_id {
        stripped = stripped.replace(&format!("<@&{role_id}>"), "");
    }
    stripped.trim().to_owned()
}
fn is_status_question(text: &str) -> bool {
    matches!(
        text.trim()
            .trim_end_matches(['?', '!', '.'])
            .to_ascii_lowercase()
            .as_str(),
        "status"
            | "status please"
            | "any update"
            | "any updates"
            | "how's it going"
            | "where are we"
            | "what are you working on"
    )
}

fn discord_attachments(message: &Message) -> Result<Vec<Attachment>> {
    message
        .attachments
        .iter()
        .map(|attachment| {
            Ok(Attachment {
                url: Url::parse(&attachment.url).with_context(|| {
                    format!(
                        "Discord returned an invalid URL for {:?}",
                        attachment.filename
                    )
                })?,
                filename: attachment.filename.clone(),
                size: u64::from(attachment.size),
                content_type: attachment.content_type.clone(),
            })
        })
        .collect()
}

fn find_bug_report_id_in_alert(content: &str, public_url: &Url) -> Option<Uuid> {
    let alert_url = content
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .trim_matches(['<', '>']);
    bug_report_id_from_url(alert_url, public_url)
}

fn find_bug_report_id_in_text(content: &str, public_url: &Url) -> Option<Uuid> {
    let scheme = format!("{}://", public_url.scheme());
    for token in content.split_whitespace() {
        for (start, _) in token.match_indices(&scheme) {
            let prefix = &token[..start];
            if prefix.contains("://") || !is_bug_report_link_prefix(prefix) {
                continue;
            }
            let url =
                token[start..].trim_end_matches(['>', ')', ']', '}', '.', ',', '!', ';', ':']);
            if let Some(id) = bug_report_id_from_url(url, public_url) {
                return Some(id);
            }
        }
    }
    None
}

fn is_bug_report_link_prefix(prefix: &str) -> bool {
    prefix.is_empty()
        || prefix
            .chars()
            .all(|character| matches!(character, '<' | '(' | '[' | '{' | '\'' | '"'))
        || prefix.ends_with("](")
}

fn bug_report_id_from_url(url: &str, public_url: &Url) -> Option<Uuid> {
    let parsed = Url::parse(url).ok()?;
    if parsed.origin() != public_url.origin()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let id = parsed.path().strip_prefix(BUG_REPORT_PATH)?;
    (id.len() == 36 && !id.contains('/'))
        .then(|| Uuid::parse_str(id).ok())
        .flatten()
}

fn find_game_id(content: &str, public_url: &Url) -> Option<Uuid> {
    for line in content.lines() {
        let line = line.trim();
        if let Some(id) = parse_hyphenated_uuid(line) {
            return Some(id);
        }

        let tokens: Vec<_> = line.split_whitespace().collect();
        for (index, token) in tokens.iter().enumerate() {
            if let Some(id) = game_id_from_url_token(token, public_url) {
                return Some(id);
            }
            if let Some(id) = labeled_game_id(token, tokens.get(index + 1).copied()) {
                return Some(id);
            }
        }
    }
    None
}

fn game_id_from_url_token(token: &str, public_url: &Url) -> Option<Uuid> {
    let candidate =
        token.trim_matches(['<', '>', '(', ')', '[', ']', '{', '}', '\'', '"', ',', '.']);
    let parsed = Url::parse(candidate).ok()?;
    if parsed.origin() != public_url.origin()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || parsed.query().is_some_and(|query| query != "post-game")
    {
        return None;
    }

    let path = parsed.path().trim_end_matches('/');
    let segments: Vec<_> = path.strip_prefix('/')?.split('/').collect();
    if !(segments.len() == 2 || segments.len() == 3)
        || segments[0] != "games"
        || segments.get(2).is_some_and(|tab| tab.is_empty())
    {
        return None;
    }
    let pretty_id = segments[1];
    if pretty_id.len() != 22 {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(pretty_id).ok()?;
    Uuid::from_slice(&decoded).ok()
}

fn labeled_game_id(token: &str, next: Option<&str>) -> Option<Uuid> {
    let token = token.trim_matches(['<', '>', '(', ')', '[', ']', '{', '}', '\'', '"', ',']);
    if let Some((label, value)) = token.split_once([':', '='])
        && is_game_label(label)
    {
        if let Some(id) = parse_uuid_token(value) {
            return Some(id);
        }
        return next.and_then(parse_uuid_token);
    }

    is_game_label(token.trim_end_matches([':', '=']))
        .then(|| next.and_then(parse_uuid_token))
        .flatten()
}

fn is_game_label(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "game" | "game-id" | "game_id" | "gameid"
    )
}

fn parse_uuid_token(value: &str) -> Option<Uuid> {
    parse_hyphenated_uuid(
        value.trim_matches(['<', '>', '(', ')', '[', ']', '{', '}', '\'', '"', ',', '.']),
    )
}

fn parse_hyphenated_uuid(value: &str) -> Option<Uuid> {
    (value.len() == 36)
        .then(|| Uuid::parse_str(value).ok())
        .flatten()
}
fn title_for(kind: RunKind, bug_report_id: Option<Uuid>, content: &str) -> String {
    if let Some(id) = bug_report_id {
        return format!("Bug report {id}");
    }
    let first_line = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Staff diagnostic request");
    let label = match kind {
        RunKind::BugReport => "Bug report",
        RunKind::StaffRequest => "Staff request",
    };
    format!("{label}: {}", truncate(first_line, 100))
}

fn initial_status(run_id: Uuid, source_url: &str, run_url: Option<&Url>) -> String {
    let inspector = run_url.map_or_else(String::new, |url| format!(" · [inspect run]({url})"));
    format!(
        "⏳ **Adjutant**: queued for diagnosis.\nrun `{run_id}` · [source]({source_url}){inspector}"
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
    fn context_budget_preserves_structural_ids_and_valid_json() {
        let id = Uuid::now_v7();
        let context = json!({
            "current_message": {"content": "\u{1f600}".repeat(8000)},
            "recent_messages": (0..12).map(|_| json!({"content": "\u{1f600}".repeat(1000)})).collect::<Vec<_>>(),
            "active_investigations": [{"run_id":id.to_string()}]
        });
        let encoded = bounded_conversation_context(context).unwrap();
        assert!(encoded.len() <= 40 * 1024);
        let decoded: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(
            decoded["active_investigations"][0]["run_id"],
            id.to_string()
        );
    }

    #[test]
    fn steering_requires_a_link_and_keeps_artifacts_on_the_collector_path() {
        let public_url = Url::parse("https://shieldbattery.invalid").unwrap();
        let link = RunLink {
            run_id: Uuid::now_v7().to_string(),
            conversation_id: Uuid::now_v7().to_string(),
        };
        let mut message = Message::default();
        message.content = "actually, this started around 03:00 UTC".to_owned();
        assert!(can_steer_message(&message, Some(&link), &public_url));
        assert!(!can_steer_message(&message, None, &public_url));
        message.content = "  ".to_owned();
        assert!(!can_steer_message(&message, Some(&link), &public_url));
        message.content = format!("check game_id {}", Uuid::now_v7());
        assert!(!can_steer_message(&message, Some(&link), &public_url));
        message.content = format!(
            "please inspect https://shieldbattery.invalid/admin/bug-reports/{}",
            Uuid::now_v7()
        );
        assert!(!can_steer_message(&message, Some(&link), &public_url));
        message.content = "extra logs".to_owned();
        message.attachments.push(
            serde_json::from_value(json!({
                "id":"1", "filename":"logs.zip", "size":42,
                "url":"https://cdn.discordapp.com/attachments/1/2/logs.zip",
                "proxy_url":"https://cdn.discordapp.com/attachments/1/2/logs.zip"
            }))
            .unwrap(),
        );
        assert!(!can_steer_message(&message, Some(&link), &public_url));
    }

    #[test]
    fn direct_mentions_and_replies_are_attention_not_role_or_everyone_pings() {
        let mut message = Message::default();
        message.content = "Adjutant might be useful here".to_owned();
        assert!(!directly_addressed(&message, 42, None));
        message.mention_everyone = true;
        assert!(!directly_addressed(&message, 42, None));
        let mut user = serenity::all::User::default();
        user.id = serenity::all::UserId::new(42);
        message.mentions.push(user.clone());
        assert!(directly_addressed(&message, 42, None));
        assert!(!directly_addressed(&message, 0, None));
        message.mentions.clear();
        let mut reference = Message::default();
        reference.author = user;
        message.referenced_message = Some(Box::new(reference));
        assert!(directly_addressed(&message, 42, None));
        assert!(!directly_addressed(&message, 43, None));
    }

    #[test]
    fn configured_role_mentions_are_attention_and_strip_for_fast_status() {
        let mut message = Message::default();
        message.content = "<@&77> status?".to_owned();
        message.mention_roles.push(serenity::all::RoleId::new(77));
        assert!(directly_addressed(&message, 0, Some(77)));
        assert!(!directly_addressed(&message, 0, None));
        assert_eq!(without_mention(&message.content, 0, Some(77)), "status?");
        assert_eq!(
            without_mention("<@&78> status?", 0, Some(77)),
            "<@&78> status?"
        );
    }

    #[test]
    fn unconfigured_or_other_role_mentions_are_not_attention() {
        let mut message = Message::default();
        message.mention_everyone = true;
        message.mention_roles.push(serenity::all::RoleId::new(77));
        assert!(!directly_addressed(&message, 42, None));
        assert!(!directly_addressed(&message, 42, Some(78)));
    }

    #[test]
    fn status_fast_path_does_not_consume_diagnostic_questions() {
        assert!(is_status_question(&without_mention(
            "<@42> status?",
            42,
            None
        )));
        assert!(is_status_question(&without_mention(
            "<@!42> any updates?",
            42,
            None
        )));
        assert!(!is_status_question(
            "check the server status for yesterday's disconnect"
        ));
        assert!(!is_status_question(
            "status of the relay and investigate the crash"
        ));
        assert_eq!(without_mention("<@43> status", 42, None), "<@43> status");
    }

    #[test]
    fn extracts_bug_report_ids_from_staff_text() {
        let first = Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap();
        let second = Uuid::parse_str("00000000-0000-4000-8000-000000000002").unwrap();
        let public_url = Url::parse("https://shieldbattery.invalid").unwrap();
        let url = format!("https://shieldbattery.invalid/admin/bug-reports/{first}");

        for content in [
            format!("<@42> look into this please: {url}"),
            format!("before <{url}>, after"),
            format!("[bug report]({url})."),
            format!("before this line\n({url})\nafter this line"),
        ] {
            assert_eq!(
                find_bug_report_id_in_text(&content, &public_url),
                Some(first)
            );
        }
        assert_eq!(
            find_bug_report_id_in_text(
                &format!(
                    "first {url} then https://shieldbattery.invalid/admin/bug-reports/{second}"
                ),
                &public_url,
            ),
            Some(first),
        );
        assert_eq!(
            find_bug_report_id_in_text(&first.to_string(), &public_url),
            None
        );

        let rejected = [
            "https://shieldbattery.invalid/admin/bug-reports/00000000000040008000000000000001"
                .to_owned(),
            format!("https://evil.example/admin/bug-reports/{first}"),
            format!("https://shieldbattery.invalid/admin/bug-reports/{first}/extra"),
            format!("https://user@shieldbattery.invalid/admin/bug-reports/{first}"),
            format!("https://shieldbattery.invalid/admin/bug-reports/{first}?unexpected"),
            format!("https://shieldbattery.invalid/admin/bug-reports/{first}#fragment"),
            format!(
                "https://evil.example/?redirect=https://shieldbattery.invalid/admin/bug-reports/{first}"
            ),
        ];
        for content in rejected {
            assert_eq!(
                find_bug_report_id_in_text(&content, &public_url),
                None,
                "unexpectedly accepted {content}",
            );
        }
    }

    #[test]
    fn automatic_bug_alert_uses_only_the_final_nonempty_line() {
        let body_id = Uuid::parse_str("00000000-0000-4000-8000-000000000003").unwrap();
        let final_id = Uuid::parse_str("00000000-0000-4000-8000-000000000004").unwrap();
        let public_url = Url::parse("https://shieldbattery.invalid").unwrap();
        let body_url = format!("https://shieldbattery.invalid/admin/bug-reports/{body_id}");
        let final_url = format!("https://shieldbattery.invalid/admin/bug-reports/{final_id}");

        assert_eq!(
            find_bug_report_id_in_alert(
                &format!("submitted text: {body_url}\n<{final_url}>"),
                &public_url
            ),
            Some(final_id),
        );
        assert_eq!(
            find_bug_report_id_in_alert(
                &format!("submitted text: {body_url}\nnot a report link"),
                &public_url
            ),
            None,
        );
    }

    #[test]
    fn extracts_game_ids_from_canonical_urls() {
        let game_id = Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0ba").unwrap();
        let other_id = Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0bb").unwrap();
        let pretty_id = URL_SAFE_NO_PAD.encode(game_id.as_bytes());
        let other_pretty_id = URL_SAFE_NO_PAD.encode(other_id.as_bytes());
        let public_url = Url::parse("https://shieldbattery.net").unwrap();

        assert_eq!(
            find_game_id(
                &format!("please inspect <https://shieldbattery.net/games/{pretty_id}>"),
                &public_url,
            ),
            Some(game_id),
        );
        assert_eq!(
            find_game_id(
                &format!(
                    "please inspect https://shieldbattery.net/games/{pretty_id}/results?post-game"
                ),
                &public_url,
            ),
            Some(game_id),
        );
        assert_eq!(
            find_game_id(
                &format!(
                    "https://shieldbattery.net/games/{pretty_id} then https://shieldbattery.net/games/{other_pretty_id}"
                ),
                &public_url,
            ),
            Some(game_id),
        );
    }

    #[test]
    fn accepts_only_explicit_raw_game_ids() {
        let game_id = Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0ba").unwrap();
        let public_url = Url::parse("https://shieldbattery.net").unwrap();

        assert_eq!(
            find_game_id(&game_id.to_string(), &public_url),
            Some(game_id)
        );
        assert_eq!(
            find_game_id(&format!("game: {game_id}"), &public_url),
            Some(game_id),
        );
        assert_eq!(
            find_game_id(&format!("gameId={game_id}"), &public_url),
            Some(game_id),
        );
        assert_eq!(
            find_game_id(&format!("report {game_id}"), &public_url),
            None,
        );
    }

    #[test]
    fn rejects_noncanonical_or_untrusted_game_urls() {
        let game_id = Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0ba").unwrap();
        let pretty_id = URL_SAFE_NO_PAD.encode(game_id.as_bytes());
        let public_url = Url::parse("https://shieldbattery.net").unwrap();
        let rejected = [
            format!("https://evil.example/games/{pretty_id}"),
            format!("https://user@shieldbattery.net/games/{pretty_id}"),
            format!("https://shieldbattery.net/games/{pretty_id}?unexpected"),
            format!("https://shieldbattery.net/games/{pretty_id}#fragment"),
            format!("https://shieldbattery.net/games/{pretty_id}/results/extra"),
            "https://shieldbattery.net/games/too-short".to_owned(),
            format!("https://shieldbattery.net/admin/bug-reports/{game_id}"),
        ];

        for value in rejected {
            assert_eq!(
                find_game_id(&value, &public_url),
                None,
                "unexpectedly accepted {value}",
            );
        }
    }

    #[test]
    fn creates_bounded_titles_on_character_boundaries() {
        let title = title_for(RunKind::StaffRequest, None, &"😀".repeat(101));
        assert_eq!(
            title.chars().count(),
            "Staff request: ".chars().count() + 101
        );
        assert!(title.ends_with('…'));
    }
}
