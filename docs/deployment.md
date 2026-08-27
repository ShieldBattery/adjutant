# Deployment runbook

The tracked `deployment/` directory is a self-contained runtime bundle: copy it to a Linux VM and
pull the application images from GHCR. The Rust source and Dockerfile are not required on the VM,
but a ShieldBattery checkout must exist at the absolute host path configured in `deployment/.env`.

Tailscale runs inside the Compose stack: its sidecar provides private ShieldBattery connectivity
and exposes the inspector through Tailnet-only HTTPS. No Tailscale host installation or published
application port is required. The host must provide Docker access to `/dev/net/tun`. Run the
commands below from the copied deployment directory (normally `/opt/adjutant`).

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

## 2. Copy and configure the deployment bundle

The GitHub workflow publishes the Dockerfile's `runtime` and `mcp-runtime` stages as
`ghcr.io/<owner>/<repository>` and `ghcr.io/<owner>/<repository>-mcp`. Copy `deployment/` without
any runtime env files; [its local README](../deployment/README.md) contains an `rsync` example and
the short update procedure.

```sh
cp .env.example .env
cp adjutant.env.example adjutant.env
cp mcp.env.example mcp.env
cp tailscale.env.example tailscale.env
chmod 600 .env adjutant.env mcp.env tailscale.env
```

Set the two GHCR image references and absolute ShieldBattery checkout path in `.env`. Fill in the
Discord token, guild/channel IDs, exact bug-report webhook ID, public ShieldBattery origin, and a
random UI password of at least 32 bytes in `adjutant.env`; put the dedicated read-only PostgreSQL
URL in `mcp.env`. For example:

```sh
openssl rand -hex 32
```

Leave `SHIELDBATTERY_INTERNAL_URL` empty until the internal API in
[the developer handoff](shieldbattery-internal-api.md) has been implemented. One-off staff requests
with Discord attachments work without it; automatic bug-report ZIP retrieval does not. Once the API
exists, set it to the app server's directly Tailscale-reachable origin. Tailscale ACLs are the
authorization boundary; there is no second application bearer token.

In the Tailscale admin console, enable MagicDNS and HTTPS, then generate a pre-authorized auth key
for this long-lived node and put it in `tailscale.env`. Prefer a tagged node such as
`tag:adjutant`; define its tag owner first and put `--advertise-tags=tag:adjutant` in
`TS_EXTRA_ARGS`. Tailnet policy should allow staff to reach this node on port 443, allow only
approved staff/developers to reach its database MCP on port 8443, and allow this node to reach only
ShieldBattery's private app-server/database ports and any explicitly required MCP endpoints.

Create the database role and curated views described in [the database MCP guide](database-mcp.md).
The MCP container is the only service that receives `mcp.env`; Codex and the Discord bot never see
the database password.

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
`tailscale.env`; retain it only if you want automatic recovery after deliberately deleting the
state volume.

Set `ADJUTANT_UI_BASE_URL` in `adjutant.env` to the HTTPS `*.ts.net` URL shown by
`tailscale serve status` so Discord status messages link directly to the matching inspected run.
Put that same exact FQDN, without a scheme or port, in `mcp.env` as
`ADJUTANT_MCP_TAILSCALE_HOSTNAME`; the MCP HTTP transport uses it for Host-header validation on the
developer endpoint. Leave it empty when using the agent-only Serve configuration.

## 3. Pull images and authenticate Codex

```sh
docker compose pull
docker compose up -d tailscale adjutant-mcp
docker compose logs --tail=100 adjutant-mcp
docker compose run --rm adjutant codex login --device-auth
docker compose run --rm adjutant codex login status
```

GHCR initially creates packages as private. For private packages, run
`docker login ghcr.io --username <github-user>` first with a classic deployment token that has only
`read:packages`; packages made public in GitHub's package settings need no registry login. Keep
this credential in Docker's credential store, not in the deployment env files.

The `codex-home` named volume retains the login state across container updates. Compose's fixed
project name is `adjutant`, so replacing or moving the bundle continues to use the same named
volumes.

Codex supports ChatGPT subscription login, and its documented headless flow is
`codex login --device-auth`. OpenAI recommends API keys as the default for automation; this setup
deliberately uses ChatGPT-managed authentication on a private, trusted runner to match Adjutant's
deployment goal. Treat the volume's `auth.json` as a password.

Do not override the image entrypoint for login or maintenance commands. It performs the one-way
drop from the root init shim to the unprivileged `adjutant` account before running the supplied
command.

The `-p`/`--profile` option selects a Codex configuration profile; it is not a prompt option.
Adjutant supplies the prompt over stdin to `codex exec -`. The CLI's JSONL mode is what powers the
run inspector. Compose mounts
[`deployment/config/codex.toml`](../deployment/config/codex.toml) read-only into the Codex home, so
the bundled database MCP and its tool allowlist are deployed as reviewed configuration; the named
volume still owns the private login state.

References: [Codex authentication](https://developers.openai.com/codex/auth),
[non-interactive mode](https://developers.openai.com/codex/noninteractive), and
[CLI command reference](https://developers.openai.com/codex/cli/reference).

## 4. Configure production tools

The bundled database MCP is required and exposes only schema discovery plus bounded read-only
queries. Add read-only Datadog or internal telemetry servers to the checked-in Codex configuration.
Mark mandatory MCPs as required so a diagnosis fails visibly instead of silently continuing
without production evidence.

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
docker compose logs -f tailscale adjutant-mcp adjutant
docker compose exec tailscale tailscale serve status
```

Use the HTTPS `*.ts.net` URL on port 443 printed by Tailscale. The browser will request HTTP Basic
credentials: the username is `adjutant`, and the password is `ADJUTANT_UI_TOKEN`. `/healthz` is the
only unauthenticated endpoint. Keep a tailnet ACL around the sidecar even though the UI also
requires the password.

The checked-in Serve configuration explicitly disables Funnel. The inspector binds only to the
loopback interface shared with the sidecar, so port 8080 is unavailable from both the host and the
Tailnet. Do not add a published port, enable Funnel, or put the inspector behind a public reverse
proxy.

The same hostname exposes the MCP to approved Tailnet developers at
`https://<hostname>.<tailnet>.ts.net:8443/mcp`. It has no second bearer token: Tailnet identity and
ACLs are the authorization layer, and the curated database views are the data boundary. Funnel is
disabled on both ports. To keep MCP access local to Adjutant, set
`TAILSCALE_SERVE_CONFIG=serve-agent-only.json` in `.env` and recreate the Tailscale service.

## Operations

- Upgrade application images: run
  `docker compose pull && docker compose up -d --remove-orphans`. The workflow publishes `latest`,
  branch, semantic-version, and `sha-*` tags; use matching `sha-*` tags or image digests for a
  controlled deployment and rollback.
- Upgrade deployment configuration: copy the new tracked contents of `deployment/` over the VM
  directory without replacing `.env`, `adjutant.env`, `mcp.env`, or `tailscale.env`, then run the
  same pull/up commands.
- Stop: `docker compose down`. Compose gives active jobs up to 35 minutes to drain.
- Back up: snapshot `adjutant-data` for requests, manifests, event JSONL, and final reports. Back up
  `tailscale-state` if preserving the node identity matters. Extracted client bundles are not
  persisted.
- Re-authenticate: rerun `docker compose run --rm adjutant codex login --device-auth`.
- Database MCP: inspect `docker compose logs adjutant-mcp`; `/healthz` checks that the dedicated
  role can connect. Agent-initiated MCP calls and results also appear in the corresponding Codex
  run's JSONL audit stream; service logs record bounded query metadata without database credentials.
- Network status: `docker compose exec tailscale tailscale status`; health for all three services is
  visible in `docker compose ps`. Adjutant's health check covers both its UI and the sidecar's local
  health endpoints. This verifies that the node has a Tailnet IP and that the database login can
  connect, not end-to-end ShieldBattery
  reachability; use a separate synthetic check if that path needs proactive alerting.
- Retention: `RUN_RETENTION_DAYS` is applied at startup. Client logs and dumps live only in per-run
  temporary storage and are removed after the process finishes.
- Limits: if `JOB_TIMEOUT_SECONDS` is raised above 1800, raise Compose's `stop_grace_period` by at
  least the same amount. Queued jobs are failed promptly on shutdown; only active jobs drain.
