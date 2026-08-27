# Deployment runbook

This runbook assumes Adjutant and ShieldBattery are sibling directories on a Linux VM. Tailscale
runs inside the Compose stack: its sidecar provides private ShieldBattery connectivity and exposes
the inspector through Tailnet-only HTTPS. No Tailscale host installation or published application
port is required. The host must provide Docker access to `/dev/net/tun`.

## 1. Create the Discord application

In the Discord Developer Portal:

1. Create an application and upload [`avatar.jpg`](../avatar.jpg) as its icon.
2. Add a bot and enable the privileged **Message Content Intent**.
3. Record the ID of ShieldBattery's existing bug-report webhook; Adjutant accepts no other webhook
   in that channel.
4. Install the bot into the ShieldBattery guild with the `bot` scope and these permissions in the three
   configured channels: View Channel, Send Messages, Attach Files, and Read Message History.
5. Give the bot access to the existing bug-report alert channel and the private request/output
   channels. The request and output IDs may point at the same private channel.

Adjutant accepts automatic bug alerts only when they arrive through a webhook in the configured
bug-report channel. It ignores bot/webhook traffic in the staff request channel. If
`DISCORD_ALLOWED_ROLE_IDS` is set, a staff request also needs one of those role IDs; otherwise the
private channel ACL is the authorization boundary.

## 2. Configure secrets and IDs

```sh
cp .env.example .env
cp .env.tailscale.example .env.tailscale
chmod 600 .env .env.tailscale
```

Fill in the Discord token, guild/channel IDs, exact bug-report webhook ID, public ShieldBattery
origin, and a random UI password of at least 32 bytes. For example:

```sh
openssl rand -hex 32
```

Leave `SHIELDBATTERY_INTERNAL_URL` empty until the internal API in
[the developer handoff](shieldbattery-internal-api.md) has been implemented. One-off staff requests
with Discord attachments work without it; automatic bug-report ZIP retrieval does not. Once the API
exists, set it to the app server's directly Tailscale-reachable origin. Tailscale ACLs are the
authorization boundary; there is no second application bearer token.

In the Tailscale admin console, enable MagicDNS and HTTPS, then generate a pre-authorized auth key
for this long-lived node and put it in `.env.tailscale`. Prefer a tagged node such as
`tag:adjutant`; define its tag owner first and put `--advertise-tags=tag:adjutant` in
`TS_EXTRA_ARGS`. Tailnet policy should allow staff to reach this node on port 443 and allow this node
to reach only ShieldBattery's private app-server port and any explicitly required MCP endpoints.

Bring up the sidecar first:

```sh
test -c /dev/net/tun
docker compose up -d tailscale
docker compose logs --tail=100 tailscale
docker compose exec tailscale tailscale status
docker compose exec tailscale tailscale serve status
```

The named `tailscale-state` volume preserves the node identity, and `TS_AUTH_ONCE=true` avoids
re-authenticating it on each restart. Once the sidecar is healthy, the auth key can be removed from
`.env.tailscale`; retain it only if you want automatic recovery after deliberately deleting the
state volume.

Set `ADJUTANT_UI_BASE_URL` in `.env` to the HTTPS `*.ts.net` URL shown by `tailscale serve status`
so Discord status messages link directly to the matching inspected run.

## 3. Build and authenticate Codex

```sh
docker compose build
docker compose run --rm adjutant codex login --device-auth
docker compose run --rm adjutant codex login status
```

The `codex-home` named volume retains the login and Codex configuration across container updates.
Codex supports ChatGPT subscription login, and its documented headless flow is
`codex login --device-auth`. OpenAI recommends API keys as the default for automation; this setup
deliberately uses ChatGPT-managed authentication on a private, trusted runner to match Adjutant's
deployment goal. Treat the volume's `auth.json` as a password.

Do not override the image entrypoint for login or maintenance commands. It performs the one-way
drop from the root init shim to the unprivileged `adjutant` account before running the supplied
command.

The `-p`/`--profile` option selects a Codex configuration profile; it is not a prompt option.
Adjutant supplies the prompt over stdin to `codex exec -`. The CLI's JSONL mode is what powers the
run inspector.

References: [Codex authentication](https://developers.openai.com/codex/auth),
[non-interactive mode](https://developers.openai.com/codex/noninteractive), and
[CLI command reference](https://developers.openai.com/codex/cli/reference).

## 4. Configure production tools

Use the dedicated `codex-home` volume for a minimal `config.toml` and MCP registrations. Only add
read-only Datadog, database, or internal telemetry tools. Mark mandatory MCPs as required so a
diagnosis fails visibly instead of silently continuing without production evidence.

`CODEX_ENV_PASSTHROUGH` is the only path for extra environment variables into the Codex process.
Adjutant rejects its Discord and UI secrets even if listed. Prefer short-lived or narrowly scoped
MCP credentials, and do not give the agent a database principal capable of writes. The source tree
is mounted read-only and Codex itself is always invoked with the read-only sandbox. That sandbox
also denies networking to model-generated commands, which is a required boundary because the parent
container shares the sidecar's Tailnet connection. Do not configure a custom network-enabled Codex
permission profile for this deployment.

## 5. Start and privately expose the UI

```sh
docker compose up -d
docker compose ps
docker compose logs -f tailscale adjutant
docker compose exec tailscale tailscale serve status
```

Use the HTTPS `*.ts.net` URL printed by Tailscale. The browser will request HTTP Basic credentials:
the username is `adjutant`, and the password is `ADJUTANT_UI_TOKEN`. `/healthz` is the only
unauthenticated endpoint. Keep a tailnet ACL around the sidecar even though the UI also requires the
password.

The checked-in Serve configuration explicitly disables Funnel. The inspector binds only to the
loopback interface shared with the sidecar, so port 8080 is unavailable from both the host and the
Tailnet. Do not add a published port, enable Funnel, or put the inspector behind a public reverse
proxy.

## Operations

- Upgrade: pull, review the pinned Tailscale/Codex/minidump versions, then run
  `docker compose build --pull && docker compose up -d`.
- Stop: `docker compose down`. Compose gives active jobs up to 35 minutes to drain.
- Back up: snapshot `adjutant-data` for requests, manifests, event JSONL, and final reports. Back up
  `tailscale-state` if preserving the node identity matters. Extracted client bundles are not
  persisted.
- Re-authenticate: rerun `docker compose run --rm adjutant codex login --device-auth`.
- Network status: `docker compose exec tailscale tailscale status`; health for both containers is
  visible in `docker compose ps`. Adjutant's health check covers both its UI and the sidecar's local
  health endpoint. This verifies that the node has a Tailnet IP, not end-to-end ShieldBattery
  reachability; use a separate synthetic check if that path needs proactive alerting.
- Retention: `RUN_RETENTION_DAYS` is applied at startup. Client logs and dumps live only in per-run
  temporary storage and are removed after the process finishes.
- Limits: if `JOB_TIMEOUT_SECONDS` is raised above 1800, raise Compose's `stop_grace_period` by at
  least the same amount. Queued jobs are failed promptly on shutdown; only active jobs drain.
