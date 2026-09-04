use std::sync::Arc;

use anyhow::{Context as _, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
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
        let game_id = find_game_id(&message.content, &self.config.shieldbattery_public_url);
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
        self.submit(context, message, kind, bug_report_id, game_id)
            .await
    }

    async fn submit(
        &self,
        context: &Context,
        message: &Message,
        kind: RunKind,
        bug_report_id: Option<Uuid>,
        game_id: Option<Uuid>,
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
            game_id,
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
