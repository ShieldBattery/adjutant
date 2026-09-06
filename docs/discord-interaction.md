# Staff conversations and investigation memory

Adjutant reads exactly two configured guild channels: staff-alerts and command-center.
Set `DISCORD_REQUEST_CHANNEL_ID` and `DISCORD_OUTPUT_CHANNEL_ID` to command-center's same ID,
and `DISCORD_BUG_REPORT_CHANNEL_ID` to staff-alerts. Startup rejects a third output channel.

## Participation

The configured application webhook in staff-alerts starts automatic bug-report diagnoses;
their acknowledgements and results appear in command-center with a link to the alert.
Other bots and webhooks never trigger a conversation. Human staff can address Adjutant in
either channel. An explicit user mention, a reply to Adjutant, or an optional configured
`DISCORD_MENTION_ROLE_ID` mention gets an immediate acknowledgement. That role only controls
addressing; it does not grant access. Ordinary messages go through a short Astra routing decision,
which can stay quiet, answer briefly, ask a clarification, report status, or start an investigation.
Ordinary staff chatter should remain ordinary chatter. Enable Discord's privileged Message Content
Intent for this judgment.

Routing has two concurrent slots and a deadline of at most 60 seconds. During saturation,
explicit requests get a busy response and passive messages may be skipped. A routing failure
does not start an investigation. The short routing process has no shell, subagents, database,
or Datadog tools; it can use the read-only staff context tools.

Replies use Discord's native message references, never threads. An acknowledgement stays
visible, and conversational answers, clarifications, status answers, meaningful progress, terminal
notices, and final reports arrive as fresh replies to the original triggering staff message with
that person's notification disabled. Starting an investigation creates a separate queued dashboard
reply. That dashboard is the editable queued, running, latest-note, and terminal snapshot. Every
run-associated acknowledgement, dashboard, progress notice, terminal notice, and report is linked
to the investigation so a reply to any of them continues the same conversation. Automatic
bug-report diagnoses post in command-center and use their dashboard as the reply target because a
Discord reply cannot cross channels. Separate conversations can run concurrently; diagnostic
follow-ups in the same conversation queue in order. A greeting or clarification does not consume a
diagnostic slot. All outbound messages suppress automatic user, role, and everyone mentions.

## Progress and status

The acknowledgement remains visible while a separate dashboard tracks queued, running, latest
note, and terminal state. While investigating, Adjutant can emit a public reply containing a
finding, next check, or blocker. The service checks for changed notes every 10 seconds and updates
the dashboard. It waits 120 seconds before the first standalone progress reply, leaves at least
120 seconds between later attempts, and makes at most three standalone progress-post attempts per
run. It coalesces pending changes into the latest note. A failed send leaves the pending note
available for a later attempt within that allowance. Timed-out sends count toward the cap because
Discord may already have received the message. It never generates timer-based "still working" messages or invented
completion percentages. Successful reports are the only completion reply; failures, cancellations,
queue rejection, panic recovery, and report-delivery failure each update the dashboard and attempt
one concise terminal reply.

Mention Adjutant with `status?` or reply to its investigation with `any updates?` for an immediate
status lookup that bypasses model routing. More specific natural-language questions can use the
short router to select a known run. Status shows queued/running/terminal state, the last public
note and observed tool activity with their separate timestamps. These are observations, not
claims about the model's hidden thoughts. A status request does not wait for a diagnostic slot.

## History and durable findings

Each routing request includes a small recent-message window and its reply context. The local
staff context MCP can page backward through all history Discord makes available in either of
the two channels, or fetch a specific message. It validates the configured guild and channel
type before live reads. Links to other channels do not grant access. Live reads include bounded
text, embed content, and attachment metadata; they do not download attachments.

`search_staff_history` searches the bounded local cache, not Discord's entire history. The cache
holds at most 10,000 messages for up to 90 days; gateway edits and deletes invalidate cached
text while connected. Historical reads refresh it. An edit or deletion missed during downtime
can remain cached until a refresh or expiry, so check the source when accuracy matters.

Completed diagnoses automatically create case notes in the existing persistent SQLite volume.
Notes include the original request link, run/conversation IDs, date, and a bounded final-report
excerpt. Staff replies classified as corrections preserve the original statement and source link as an unverified
observation; corrections accompany earlier findings instead of silently rewriting them.
The read-only `search_investigations`, `read_investigation`, and `investigation_status` tools
retrieve these records. Old hypotheses are historical evidence, never confirmed current facts.

Case notes, attributed observations, and reply links last 730 days. Detailed run records use
`RUN_RETENTION_DAYS`. Pruning currently runs at startup; the message cache additionally trims
on writes. A retained case can outlive its detailed run record. The `run_record_available` field
means the run record remains, not that original logs or dumps are still on disk: extracted
artifacts remain temporary. Tool responses explicitly limit lists and text. Back up the
`adjutant-data` volume to retain memory across VM replacement; replacing containers preserves it.

## Discord visibility and deployment

Give the bot View Channel, Read Message History, Send Messages, and Attach Files in only these
two staff channels. Do not grant Administrator. Review effective permissions across @everyone,
bot roles, category overrides, and channel overrides: removing a grant from one role does not
remove access granted elsewhere. Deny View Channel for the bot elsewhere and allow it explicitly
in these staff channels, checking new categories/channels as they are added. The application
also filters guild/channel IDs before caching or routing incoming messages and validates every
history-tool channel. Optional `DISCORD_MENTION_ROLE_ID` makes one role mention address
Adjutant, while `DISCORD_ALLOWED_ROLE_IDS` separately gates human requests in both channels.
Neither setting grants channel visibility; channel permissions still determine the readable staff
context.

The staff context MCP binds only to shared loopback `127.0.0.1:8083/mcp`. It is absent from
Tailscale Serve and has no published port. The Rust parent holds the Discord credential and
performs reads/writes; Codex receives no Discord credential or writable memory tool. Its command
sandbox remains read-only with networking disabled. Copy the updated `config/codex.toml` and
`config/AGENTS.md` with the deployment bundle, set the two channel IDs, then recreate Adjutant.

References: [Discord permissions](https://docs.discord.com/developers/topics/permissions),
[message replies](https://docs.discord.com/developers/resources/message), and
[gateway intents](https://docs.discord.com/developers/events/gateway).
