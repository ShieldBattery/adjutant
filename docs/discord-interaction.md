# Staff conversations and investigation memory

Adjutant reads exactly two configured guild channels: staff-alerts and command-center.
Set `DISCORD_REQUEST_CHANNEL_ID` and `DISCORD_OUTPUT_CHANNEL_ID` to command-center's same ID,
and `DISCORD_BUG_REPORT_CHANNEL_ID` to staff-alerts. Startup rejects a third output channel.

## Participation

The configured application webhook in staff-alerts starts automatic bug-report diagnoses;
their acknowledgements and results appear in command-center with a link to the alert.
Other bots and webhooks never trigger a conversation. Human staff can address Adjutant in
either channel. An explicit user mention, a reply linked to an investigation or addressed to
Adjutant, or an optional configured `DISCORD_MENTION_ROLE_ID` mention gets an immediate
acknowledgement. That role only controls
addressing; it does not grant access. Ordinary messages go through a short Astra routing decision,
which can stay quiet, answer briefly, ask a clarification, report status, steer active work,
or start an investigation.
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

Final reports start with **summary** and **next checks**, using normal-sized bold labels. The
Discord reply links to the original request, so the report does not repeat its title or links.
Confidence, evidence, and likely-cause notes appear in a compact details section. Long sections
are shortened independently so evidence cannot crowd out the next checks; the full report is
attached when the preview omits content. The inspector and investigation memory retain the original
report. Important uncertainty and blockers belong in the summary. Blocked investigations should
stay brief instead of repeating missing evidence under every heading.

## Collecting more evidence

Staff messages can include a canonical bug-report link inline with their request, on a separate
line, or as a Markdown link. Adjutant extracts its ID and retrieves the report through the
configured internal API. Automatic webhook alerts still use only their final report-link line,
so a URL inside submitted report text cannot replace the alert's actual report ID.

During an investigation, Codex can also call `request_bug_report` with a report UUID or
`request_game_artifacts` with a game UUID. The latter collects the available map file, replays,
flight recordings, and artifact metadata. `get_game_diagnostics` on the database MCP provides
game details, participants, results, map metadata, and netcode history.

These evidence tools run through the host's existing internal API client. They accept typed IDs,
return local paths, and publish completed files into the current workspace with an updated
manifest in the inspector. Repeated IDs reuse the collected files. Initial and later collection
share archive expansion/file limits and game artifact count/byte limits. Each run allows eight
uncached collection attempts; failed requests also consume an attempt. Codex commands remain
read-only with networking disabled. Reports with expired logs can still supply their metadata.

## Updating an investigation

Reply to an investigation's acknowledgement, dashboard, progress message, original request, or
result to give it more context. For example: "actually, this started around 03:00 UTC" or
"focus on the reconnect path first." The router distinguishes relevant diagnostic updates from
status questions, greetings, unrelated requests, and requests for a separate investigation.
Adjutant acknowledges the reply immediately and confirms delivery once Codex accepts the update.
Accepted updates stay in the same run, preserve the original staff text and message attribution,
are recorded within the inspection event budget, and are saved as attributed case observations.

The service pins the active run before routing the message. A completion race cannot redirect
an update into a different run. If the target finishes or cannot accept more updates, Adjutant
queues a normal follow-up in the same conversation. Attachments, game links/explicit game IDs,
and report links also use that path so evidence downloads and archive limits still apply.
If delivery becomes uncertain after sending, Adjutant says so instead of automatically submitting
the same input twice. The inspector shows the recorded delivery outcome when available.

Each run accepts at most 16 update submissions and 128 KiB of serialized staff updates, with at
most eight waiting in its mailbox and 32 KiB per update. Updates share the investigation's
original deadline and output limits; they cannot extend its lifetime or change its sandbox,
credentials, model, or tool policy. Staff can request another investigation when more work is needed.

Diagnostics use a private stdio Codex app-server connection and an ephemeral thread. The service
uses only initialization, thread creation, turn start, and turn steering; it rejects requests for
approvals, extra permissions, or interactive tools. It exposes no app-server network listener.
Every protocol frame, including a complete final message, must fit `MAX_CODEX_EVENT_LINE_BYTES`.
An oversized frame fails the run explicitly; event-budget exhaustion stops retaining audit events
while still allowing bounded control messages and the final report to complete.

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
