# Deployment runbook

The tracked `deployment/` directory is a self-contained runtime bundle: copy it to a Linux VM and
pull the application images from GHCR. The Rust source, Dockerfile, and ShieldBattery checkouts are
not required on the VM; the bundled source synchronizer maintains them in a persistent volume.

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
4. Install the bot with the `bot` scope and View Channel, Send Messages, Attach Files, and
   Read Message History in only staff-alerts and command-center. Do not grant Administrator.
5. Set both request and output IDs to command-center and the bug-report ID to staff-alerts.
   Review effective category/channel permissions so the bot cannot view member channels.

Only the configured webhook triggers automatic alert diagnoses. Human mentions and replies get
acknowledged; Astra judges whether other staff messages warrant participation. Optional
`DISCORD_ALLOWED_ROLE_IDS` gates human requests in both channels. See the
[interaction guide](discord-interaction.md) for permissions, replies, status, history, and memory.

## 2. Copy and configure the deployment bundle

The GitHub workflow publishes the Dockerfile's `runtime` and `mcp-runtime` stages as
`ghcr.io/<owner>/<repository>` and `ghcr.io/<owner>/<repository>-mcp`. Copy `deployment/` without
any runtime env files; [its local README](../deployment/README.md) contains an `rsync` example and
the short update procedure.

```sh
cp .env.example .env
cp adjutant.env.example adjutant.env
cp datadog-mcp.env.example datadog-mcp.env
cp mcp.env.example mcp.env
cp tailscale.env.example tailscale.env
chmod 600 .env adjutant.env datadog-mcp.env mcp.env tailscale.env
```

Set the two GHCR image references and review the source-sync organization, interval, depth,
retention, and size settings in `.env`. Fill in the Discord token, guild/channel IDs, exact
bug-report webhook ID, and public ShieldBattery origin in `adjutant.env`; put the dedicated
read-only PostgreSQL URL in `mcp.env`. Put the dedicated read-only Datadog Service Access
Token and managed MCP hostname for your Datadog site in `datadog-mcp.env`. See the
[Datadog MCP guide](datadog-mcp.md) for the exact role and token setup.

`source-sync` uses no GitHub credential. It enumerates the configured organization's public
repositories, requires the `ShieldBattery` repository, skips other empty repositories, keeps
bounded-depth Git history, and publishes a new all-repository generation only after every fetch and
checkout succeeds. Compose passes the same `JOB_TIMEOUT_SECONDS` to the bot and synchronizer, and
the synchronizer rejects retention shorter than that timeout plus one synchronization interval.
The defaults retain source generations for two hours around 30-minute diagnoses. See the
[source-sync guide](source-sync.md) for its snapshot contract and operations.

Set `SHIELDBATTERY_INTERNAL_URL` to the app server's directly Tailscale-reachable origin to enable
automatic bug-report retrieval and game-artifact retrieval. A staff request containing a canonical
ShieldBattery game link, a raw game UUID on its own line, or a raw UUID labeled `game`, `game-id`,
`game_id`, or `gameId` attempts to download all retained flight recordings, replays, the map, and
artifact metadata up to the configured limits. An artifact that disappears after listing becomes
an explicit unavailable marker; other retrieval or validation failures fail the run. If the
internal URL is empty, a request naming a bug report or game fails, while attachment-only requests
still work. Tailscale ACLs are the authorization boundary, and there is no second application
bearer token.

In the Tailscale admin console, enable MagicDNS and HTTPS. Prefer a tagged node such as
`tag:adjutant`. In **Access controls**, merge this entry into the policy's `tagOwners` section
(create the section if it is absent):

```json
"tagOwners": {
  "tag:adjutant": ["autogroup:admin"]
}
```

Save the policy, then generate an auth key with **Tags: tag:adjutant**, **Ephemeral: off**, and
**Pre-approved: on** if device approval is enabled. Put the key in `tailscale.env` as `TS_AUTHKEY`.
The key assigns its tag on first login. If `TS_EXTRA_ARGS` also contains
`--advertise-tags=tag:adjutant`, that tag must be permitted by the key. An error such as
`requested tags [tag:adjutant] are invalid or not permitted` means the tag definition or credential
authorization needs correcting. After editing the env file, recreate the container with
`docker compose up -d --force-recreate --wait tailscale` to load the new values.

See [Tailscale's tag documentation](https://tailscale.com/docs/features/tags) for tag ownership and
auth key requirements. Tailnet policy should allow staff to reach this node on port 443, allow only
approved staff/developers to reach its database MCP on port 8443, and allow this node to reach only
ShieldBattery's private app-server/database ports. The VM's ordinary egress policy must also allow
DNS and TCP 443 to the selected Datadog managed MCP hostname, `api.github.com`, and `github.com`.
The GitHub traffic comes from `source-sync` on Docker's ordinary network, not from the Tailnet or
model-generated commands. The synchronizer constructs only those two GitHub destinations and
disables redirects, but Docker Compose does not enforce an FQDN allowlist; use a VM firewall or
outbound proxy if that restriction must be independently enforced.

Create the database role and curated views described in [the database MCP guide](database-mcp.md).
The MCP container is the only service that receives `mcp.env`; Codex and the Discord bot never see
the database password.

The sidecar needs writable `/run` for both its LocalAPI socket and the firewall's
`/run/xtables.lock`. Compose supplies a temporary filesystem there while keeping the root
filesystem read-only. If an older bundle reports `can't open lock file /run/xtables.lock:
Read-only file system`, update `compose.yaml` and recreate the Tailscale service. Keep the
`tailscale-state` named volume, which stores the node's identity.

Bring up the sidecar and wait for it to become healthy before starting dependent services:

```sh
test -c /dev/net/tun
docker compose up -d --wait tailscale
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
docker compose pull --policy always
docker compose up -d source-sync tailscale adjutant-mcp datadog-mcp-proxy
docker compose logs --tail=100 source-sync
docker compose logs --tail=100 adjutant-mcp
docker compose logs --tail=100 datadog-mcp-proxy
docker compose run --rm adjutant codex login --device-auth
docker compose run --rm adjutant codex login status
```

Application services use `pull_policy: missing`: startup and maintenance reuse cached images and
fetch missing ones automatically. The default `:main` tag avoids Docker's documented `:latest`
refresh exception. On an existing VM, change both application image tags in `.env` from `:latest`
to `:main` to use this behavior. Run `docker compose pull --policy always` when you want updates.

GHCR initially creates packages as private. For private packages, run
`docker login ghcr.io --username <github-user>` first with a classic deployment token that has only
`read:packages`; packages made public in GitHub's package settings need no registry login. Keep
this credential in Docker's credential store, not in the deployment env files.

The `codex-home` named volume retains the login state across container updates, while `source-repos`
retains Git mirrors and published source generations. Compose's fixed project name is `adjutant`,
so replacing or moving the bundle continues to use the same named volumes.

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

The bundled database and Datadog MCP connections are required. The former exposes reviewed
user/game diagnostic lookups, schema discovery, and bounded read-only queries; the latter exposes a
reviewed allowlist for logs, traces, metrics, events, monitors, and service relationships. The
Datadog credential proxy fixes the managed upstream and `core` toolset while keeping its token out
of Codex. Mark future mandatory MCPs as required so a diagnosis fails visibly instead of silently
continuing without production evidence.

`CODEX_ENV_PASSTHROUGH` is the only path for extra environment variables into the Codex process.
Adjutant rejects `DISCORD_TOKEN` and the legacy `ADJUTANT_UI_TOKEN` value even if listed. The latter
remains denylisted for upgrades even though the current UI does not use it. Prefer short-lived or
narrowly scoped MCP credentials, and do not give either production identity write access.
`source-sync` is the sole
writer to the source volume; the Adjutant container mounts its atomically published generations
read-only. Codex itself is always invoked with the read-only sandbox. That sandbox also denies
networking to model-generated commands, which is a required boundary because the parent container
shares the sidecar's Tailnet connection. Do not configure a custom network-enabled Codex permission
profile for this deployment.

## 5. Start and privately expose the UI

```sh
docker compose up -d
docker compose ps
docker compose logs -f source-sync tailscale adjutant-mcp datadog-mcp-proxy adjutant
docker compose exec tailscale tailscale serve status
```

Use the HTTPS `*.ts.net` URL on port 443 printed by Tailscale. Access is authorized by the Tailnet
ACLs and grants for this node; the browser does not prompt for a separate UI password. Forwarded
Tailscale identity headers are not used as an independent application authentication mechanism.
`/healthz` contains no run data and is available for health checks.

The checked-in Serve configuration explicitly disables Funnel. The inspector binds only to the
loopback interface shared with the sidecar, so port 8080 is unavailable from both the host and the
Tailnet; Tailscale Serve is the only ingress on HTTPS 443. Do not add a published port, enable
Funnel, or put the inspector behind a public reverse proxy.

Images from before this authentication change still require `ADJUTANT_UI_TOKEN`; upgrade the
application image before removing that variable from an existing deployment. Once the upgraded
image is running, the legacy value is ignored and can be removed.

The same hostname exposes the MCP to approved Tailnet developers at
`https://<hostname>.<tailnet>.ts.net:8443/mcp`. It has no second bearer token: Tailnet identity and
ACLs are the authorization layer, and the curated database views are the data boundary. Funnel is
disabled on both ports. To keep MCP access local to Adjutant, set
`TAILSCALE_SERVE_CONFIG=serve-agent-only.json` in `.env` and recreate the Tailscale service.

## Operations

- Upgrade application images: run
  `docker compose pull --policy always && docker compose up -d --remove-orphans`. The workflow
  publishes `latest`, branch, semantic-version, and `sha-*` tags; use matching `sha-*` tags or image digests for a
  controlled deployment and rollback.
- Upgrade deployment configuration: copy the new tracked contents of `deployment/` over the VM
  directory without replacing `.env`, `adjutant.env`, `datadog-mcp.env`, `mcp.env`, or
  `tailscale.env`. Review changed `.example` files; if the update introduces a service env file that
  is not already present, copy its example, restrict it to mode `600`, and fill in the required
  values before running the same pull/up commands. In particular, deployments upgrading to the
  Datadog MCP integration must create and fill `datadog-mcp.env` first.
- Upgrade from the old host-checkout deployment: remove the obsolete
  `SHIELDBATTERY_HOST_SOURCE_PATH` setting from `.env` when convenient. Compose creates and fills
  `source-repos` automatically; the old host checkout is no longer mounted or updated by Adjutant.
- Stop: `docker compose down`. Compose gives active jobs up to 35 minutes to drain.
- Back up: snapshot `adjutant-data` for requests, manifests, event JSONL, final reports, and durable case memory. Back up
  `tailscale-state` if preserving the node identity matters. `source-repos` is reproducible from
  public GitHub and normally does not need backup. Extracted client bundles are not persisted.
- Re-authenticate: rerun `docker compose run --rm adjutant codex login --device-auth`.
- Database MCP: inspect `docker compose logs adjutant-mcp`; `/healthz` checks that the dedicated
  role can connect. Agent-initiated MCP calls and results also appear in the corresponding Codex
  run's JSONL audit stream; service logs record bounded query metadata without database credentials.
- Datadog MCP: inspect `docker compose logs datadog-mcp-proxy`; its `/healthz` checks only that the
  credential proxy is listening. Codex marks Datadog required and therefore fails a diagnosis
  visibly if upstream initialization or authentication fails. Rotate its token using the procedure
  in the [Datadog MCP guide](datadog-mcp.md).
- Source synchronization: inspect `docker compose logs source-sync`. It discovers every non-empty
  public repository in `SOURCE_SYNC_GITHUB_ORG`, fetches on `SOURCE_SYNC_INTERVAL_SECONDS`, and
  publishes the whole organization as one consistent generation. A run records the selected commit
  IDs from `.adjutant-source-manifest.json`. Restart `source-sync` to request an immediate refresh;
  rebuilding or restarting Adjutant is unnecessary.
- Network status: `docker compose exec tailscale tailscale status`; service health is visible in
  `docker compose ps`. Adjutant's health check covers its UI and the three shared-network sidecar
  health endpoints; `source-sync` reports its own snapshot freshness separately.
  This verifies that the node has a Tailnet IP, the database login can connect, and the Datadog proxy
  is listening—not end-to-end ShieldBattery or Datadog reachability. Use separate synthetic checks
  if those paths need proactive alerting.
- Retention: `RUN_RETENTION_DAYS` is applied at startup. Client logs and dumps live only in per-run
  temporary storage and are removed after the process finishes.
- Limits: if `JOB_TIMEOUT_SECONDS` is raised above 1800, also keep
  `SOURCE_SYNC_RETENTION_SECONDS` at least one sync interval above it and raise Compose's
  `stop_grace_period` by at least the same amount. The source size settings use GitHub's
  API-reported sizes as preflight guards; monitor or quota `source-repos` for a hard disk bound.
  Queued jobs are failed promptly on shutdown; only active jobs drain.
  Game artifacts are fetched sequentially, and the end-to-end job timeout includes both evidence
  collection and diagnosis; slow multi-artifact pulls can therefore consume diagnosis time.
