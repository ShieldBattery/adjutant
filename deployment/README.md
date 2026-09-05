# Adjutant VM bundle

This directory is the complete runtime deployment bundle. Copy it to a Linux VM and pull the two
application images from GHCR; the Rust source tree and Dockerfile are not required on the VM.

The bundle intentionally does not contain secrets, persistent state, container images, or source
checkouts. Compose uses the fixed project name `adjutant`, so replacing or moving this directory
continues to use the existing `adjutant-data`, `codex-home`, `source-repos`, and `tailscale-state`
volumes.

## Diagnostic guidance

`config/AGENTS.md` defines Adjutant's voice, investigation habits, and subagent delegation guidance.
Compose mounts it read-only as `AGENTS.md` in the runtime `CODEX_HOME`; Codex loads it for each new
run. This is a sanitized deployment file, independent of an operator's personal Codex guidance.
`config/codex.toml` selects the default model and MCP tool policy. The application adds the incident
request, evidence paths, safety instructions, and required response sections through its prompt.

When updating an existing VM, copy `config/AGENTS.md` along with `compose.yaml` before recreating the
service. The bind mount requires the file to exist. Keep the private Codex home free of a local
`AGENTS.override.md`, which would take precedence over this shared guidance.

## First installation

Install Docker Engine with the Compose plugin on a Linux x86-64 VM and make `/dev/net/tun`
available. From the copied directory:

```sh
cp .env.example .env
cp adjutant.env.example adjutant.env
cp datadog-mcp.env.example datadog-mcp.env
cp mcp.env.example mcp.env
cp tailscale.env.example tailscale.env
chmod 600 .env adjutant.env datadog-mcp.env mcp.env tailscale.env
```

Set the two `ghcr.io` image names and review the source-sync and shared job-timeout settings in
`.env`, then fill the four service env files. The credential-free synchronizer discovers every
non-empty public repository in the configured GitHub organization and refreshes consistent
snapshots in a persistent volume.

`datadog-mcp.env` contains the dedicated read-only Datadog service token and its site's managed MCP
hostname.

The repository workflow publishes `ghcr.io/<owner>/<repository>` and
`ghcr.io/<owner>/<repository>-mcp`. GHCR initially creates packages as private; make both packages
public in GitHub's package settings if anonymous VM pulls are preferred. Otherwise authenticate the
deployment account with a classic GitHub token that has only `read:packages`:

```sh
docker login ghcr.io --username <github-user>
```

Keep the token in Docker's credential store; do not put it in any file in this bundle.

Enroll Tailscale and authenticate Codex before starting the full service:

```sh
test -c /dev/net/tun
docker compose config --quiet
docker compose up -d source-sync
docker compose logs --tail=100 source-sync
docker compose up -d tailscale
docker compose logs --tail=100 tailscale
docker compose up -d adjutant-mcp datadog-mcp-proxy
docker compose run --rm adjutant codex login --device-auth
docker compose up -d
docker compose ps
```

## Updating

When only the application images changed:

```sh
docker compose pull
docker compose up -d --remove-orphans
docker compose ps
```

When deployment files also changed, copy the new contents over this directory without replacing
`.env`, `adjutant.env`, `datadog-mcp.env`, `mcp.env`, or `tailscale.env`, then run the same update
commands. For example, from a source checkout:

```sh
rsync -av --delete \
  --exclude='.env' \
  --exclude='adjutant.env' \
  --exclude='datadog-mcp.env' \
  --exclude='mcp.env' \
  --exclude='tailscale.env' \
  deployment/ <vm>:/opt/adjutant/
```

Review changed `.example` files before updating. If an update introduces a service env file that
does not exist on the VM—such as `datadog-mcp.env`—copy its example, restrict it to mode `600`, and
fill in its required values before running `docker compose pull` or `docker compose up`.
Older deployments may remove `SHIELDBATTERY_HOST_SOURCE_PATH` from `.env`; source is now maintained
inside the `source-repos` named volume, and the previous host checkout is unused.

The workflow publishes `latest`, branch, semantic-version, and `sha-*` tags. For a controlled
deployment or rollback, set both image values in `.env` to the same `sha-*` revision or to exact
`@sha256:` digests, then repeat the pull/up commands.

Source updates do not require an image update. `source-sync` fetches the organization on
`SOURCE_SYNC_INTERVAL_SECONDS`, atomically publishes complete generations, and keeps recent
generations long enough for in-flight diagnoses. Restart it to request an immediate refresh:

```sh
docker compose restart source-sync
docker compose logs --tail=100 source-sync
```

The full provisioning, security, database-role, and operations notes remain in
`docs/deployment.md` and `docs/database-mcp.md` in the source repository.

Staff conversations use exactly two channels. Set `DISCORD_REQUEST_CHANNEL_ID` and
`DISCORD_OUTPUT_CHANNEL_ID` to the same command-center ID and the bug-report channel to
staff-alerts. Copy both `config/codex.toml` and `config/AGENTS.md` when upgrading. The new
staff-context MCP is internal loopback only and needs no Tailscale Serve or port changes.
Investigation case notes persist in `adjutant-data` alongside run history. The source repository's
`docs/discord-interaction.md` explains replies, quiet progress, status, history, and memory.
