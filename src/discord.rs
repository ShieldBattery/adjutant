use std::sync::Arc;

use anyhow::{Context as _, Result};
use serenity::all::{
    ChannelId, Context, CreateAllowedMentions, CreateMessage, EditMessage, EventHandler,
    GatewayIntents, Message, Ready,
};
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;

use crate::config::Config;
use crate::evidence::{Attachment, EvidenceRequest};
use crate::jobs::{DiagnosticJob, DiscordDelivery, JobQueue, status_text};
use crate::store::{NewRun, RunKind, Store};

const BUG_REPORT_PATH: &str = "/admin/bug-reports/";

pub struct DiscordHandler {
    config: Arc<Config>,
    store: Store,
    queue: JobQueue,
}

impl DiscordHandler {
    #[must_use]
    pub fn new(config: Arc<Config>, store: Store, queue: JobQueue) -> Self {
        Self {
            config,
            store,
            queue,
        }
    }

    async fn handle_message(&self, context: &Context, message: &Message) -> Result<()> {
        if message.guild_id.map(serenity::all::GuildId::get) != Some(self.config.discord_guild_id) {
            return Ok(());
        }

        let channel_id = message.channel_id.get();
        let bug_alert = channel_id == self.config.discord_bug_report_channel_id
            && message.webhook_id.map(serenity::all::WebhookId::get)
                == Some(self.config.discord_bug_report_webhook_id);
        let staff_request = channel_id == self.config.discord_request_channel_id
            && message.webhook_id.is_none()
            && !message.author.bot;
        if !bug_alert && !staff_request {
            return Ok(());
        }
        if staff_request && !self.staff_member_is_allowed(message) {
            warn!(
                user_id = message.author.id.get(),
                message_id = message.id.get(),
                "ignored diagnostic request from a user without an allowed role"
            );
            return Ok(());
        }

        let bug_report_id =
            find_bug_report_id(&message.content, &self.config.shieldbattery_public_url);
        if bug_alert && bug_report_id.is_none() && message.attachments.is_empty() {
            warn!(
                message_id = message.id.get(),
                "ignored bug-report webhook message without a report ID or attachment"
            );
            return Ok(());
        }
        if staff_request && message.content.trim().is_empty() && message.attachments.is_empty() {
            message
                .channel_id
                .send_message(
                    &context.http,
                    CreateMessage::new()
                        .content("Please include a diagnostic question or attach evidence.")
                        .allowed_mentions(CreateAllowedMentions::new()),
                )
                .await?;
            return Ok(());
        }

        let kind = if bug_alert {
            RunKind::BugReport
        } else {
            RunKind::StaffRequest
        };
        self.submit(context, message, kind, bug_report_id).await
    }

    async fn submit(
        &self,
        context: &Context,
        message: &Message,
        kind: RunKind,
        bug_report_id: Option<Uuid>,
    ) -> Result<()> {
        let title = title_for(kind, bug_report_id, &message.content);
        let request_text = if message.content.trim().is_empty() {
            "Inspect the attached evidence and diagnose the reported problem.".to_owned()
        } else {
            message.content.trim().to_owned()
        };
        let evidence = EvidenceRequest {
            author: message.author.name.clone(),
            text: request_text.clone(),
            bug_report_id,
            attachments: discord_attachments(message)?,
        };
        let run = NewRun::new(
            kind,
            title.clone(),
            request_text.clone(),
            bug_report_id,
            self.config.discord_guild_id,
            message.channel_id.get(),
            message.id.get(),
        );
        self.store.create_run(&run).await?;

        let source_url = message.link();
        let run_url = self.run_url(run.id);
        let output_channel = ChannelId::new(self.config.discord_output_channel_id);
        let status_message = match self
            .post_queued_status(
                context,
                output_channel,
                run.id,
                &source_url,
                run_url.as_ref(),
            )
            .await
        {
            Ok(status) => status,
            Err(error) => {
                self.store
                    .fail_run(run.id, "Could not post the run status in Discord")
                    .await?;
                return Err(error);
            }
        };

        let delivery = DiscordDelivery {
            http: Arc::clone(&context.http),
            output_channel,
            status_message: status_message.id,
            source_url,
            run_url,
        };
        let job = DiagnosticJob {
            run_id: run.id,
            title,
            request: evidence,
            delivery,
        };
        if let Err(queue_error) = self.queue.try_enqueue(job) {
            self.store
                .fail_run(run.id, &queue_error.to_string())
                .await?;
            let status_body = status_text(
                &DiscordDelivery {
                    http: Arc::clone(&context.http),
                    output_channel,
                    status_message: status_message.id,
                    source_url: message.link(),
                    run_url: self.run_url(run.id),
                },
                run.id,
                "❌",
                &format!("Could not queue this run: {queue_error}."),
            );
            output_channel
                .edit_message(
                    &context.http,
                    status_message.id,
                    EditMessage::new()
                        .content(status_body)
                        .allowed_mentions(CreateAllowedMentions::new()),
                )
                .await?;
        }
        Ok(())
    }

    async fn post_queued_status(
        &self,
        context: &Context,
        output_channel: ChannelId,
        run_id: Uuid,
        source_url: &str,
        run_url: Option<&Url>,
    ) -> anyhow::Result<Message> {
        output_channel
            .send_message(
                &context.http,
                CreateMessage::new()
                    .content(initial_status(run_id, source_url, run_url))
                    .allowed_mentions(CreateAllowedMentions::new()),
            )
            .await
            .map_err(Into::into)
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
            error!(
                message_id = message.id.get(),
                error = ?error,
                "failed to handle Discord message"
            );
        }
    }

    async fn ready(&self, _context: Context, ready: Ready) {
        info!(user = %ready.user.name, "Discord bot connected");
    }
}

#[must_use]
pub const fn gateway_intents() -> GatewayIntents {
    GatewayIntents::GUILD_MESSAGES.union(GatewayIntents::MESSAGE_CONTENT)
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

fn find_bug_report_id(content: &str, public_url: &Url) -> Option<Uuid> {
    let alert_url = content
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .trim_matches(['<', '>']);
    let parsed = Url::parse(alert_url).ok()?;
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
        "⏳ **Adjutant** — Queued for diagnosis.\nRun `{run_id}` · [source]({source_url}){inspector}"
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
    fn extracts_only_complete_bug_report_ids_after_admin_path() {
        let id = Uuid::parse_str("018e301c-3ca2-7524-9ce9-a76a1ee7a0bb").unwrap();
        let public_url = Url::parse("https://shieldbattery.net").unwrap();
        assert_eq!(
            find_bug_report_id(
                &format!("New report:\n<https://shieldbattery.net/admin/bug-reports/{id}>"),
                &public_url
            ),
            Some(id)
        );
        assert_eq!(find_bug_report_id(&id.to_string(), &public_url), None);
        assert_eq!(
            find_bug_report_id(
                "https://shieldbattery.net/admin/bug-reports/018e301c3ca275249ce9a76a1ee7a0bb",
                &public_url
            ),
            None
        );
        assert_eq!(
            find_bug_report_id(
                &format!("https://evil.example/admin/bug-reports/{id}"),
                &public_url
            ),
            None
        );
        assert_eq!(
            find_bug_report_id(
                &format!("https://shieldbattery.net/admin/bug-reports/{id}/extra"),
                &public_url
            ),
            None
        );
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
