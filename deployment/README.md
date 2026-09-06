# Adjutant VM bundle

This directory is the complete runtime deployment bundle. Copy it to a Linux VM and pull the two
application images from GHCR; the Rust source tree and Dockerfile are not required on the VM.

The bundle intentionally does not contain secrets, persistent state, container images, or source
checkouts. Compose uses the fixed project name `adjutant`, so replacing or moving this directory
continues to use the existing `adjutant-data`, `codex-home`, `source-repos`, and `tailscale-state`
volumes.

## Task guidance

`config/AGENTS.md` defines Adjutant's voice, investigation habits, and subagent delegation guidance.
Compose mounts it read-only as `AGENTS.md` in the runtime `CODEX_HOME`; Codex loads it for each new
run. This is a sanitized deployment file, independent of an operator's personal Codex guidance.
`config/codex.toml` selects the default model and MCP tool policy. The application adds the
request, evidence paths, safety instructions, and response guidance through its prompt. Staff tasks
include data lookups, comparisons, summaries, and diagnoses. Automatic bug-report alerts retain
explicit diagnostic guidance; other questions use only the relevant reads and response sections.

`SHIELDBATTERY_INTERNAL_URL` is the private ShieldBattery app-server origin used for evidence
collection. Adjutant collects recognized report/game links before a run, and Codex can request
additional reports or game artifacts (including maps, replays, and flight recordings) by ID while
it investigates. These tools need no extra token or exposed port. They use the same internal API
client and cumulative collection limits, and add their files to the inspector's evidence manifest.
An unset origin, HTTP failure, or exhausted limit is returned as a tool error in the run log.

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

On Ubuntu or another AppArmor host, complete [Linux sandbox permissions](#linux-sandbox-permissions)
before starting Adjutant. Then enroll Tailscale and authenticate Codex:

```sh
test -c /dev/net/tun
docker compose config --quiet
docker compose pull --policy always
docker compose up -d source-sync
docker compose logs --tail=100 source-sync
docker compose up -d tailscale
docker compose logs --tail=100 tailscale
docker compose up -d adjutant-mcp datadog-mcp-proxy
docker compose run --rm adjutant codex login --device-auth
docker compose up -d
docker compose ps
```

Application services use `pull_policy: missing`: startup and maintenance reuse cached images and
fetch missing ones automatically. The default `:main` tag avoids Docker's documented `:latest`
refresh exception. On an existing VM, change both application image tags in `.env` from `:latest`
to `:main` to use this behavior. Run `docker compose pull --policy always` when you want updates.

## Linux sandbox permissions

The `adjutant` service uses `security/seccomp-adjutant.json` to let Bubblewrap build its command
sandbox. It preserves Docker's syscall allowlist and adds the namespace/mount calls required by
the pinned Codex and packaged Bubblewrap on x86-64. Other services keep Docker's default policy.

On Ubuntu 24.04 and other AppArmor hosts, Docker's default AppArmor profile also blocks the mount
setup. Install the bundled profile from this directory:

```sh
sudo apt-get update
sudo apt-get install --yes apparmor apparmor-utils
sudo install -D -m 0644 security/apparmor-adjutant /etc/apparmor.d/adjutant-sandbox
sudo apparmor_parser -r -W /etc/apparmor.d/adjutant-sandbox
```

Add this line to the existing `.env` file to enable the AppArmor overlay for subsequent Compose
commands. If `COMPOSE_FILE` already lists overlays, append `:compose.apparmor.yaml` to that list.

```dotenv
COMPOSE_FILE=compose.yaml:compose.apparmor.yaml
```

The overlay selects `adjutant-sandbox` only for Adjutant. Hosts without AppArmor, including Docker
Desktop, use the base Compose file without this overlay. The profile permits namespace creation,
mounts, and root switching for Bubblewrap. Capability drops, `no-new-privileges`, the read-only
container root, and Codex's read-only/network-disabled command policy remain in effect. The
[profile notes](security/README.md) describe the permissions and upstream provenance.

For an existing deployment, copy the new `security/` directory and `compose.apparmor.yaml` along
with `compose.yaml`, load the profile, and enable the overlay before recreating Adjutant. Reload
the profile with the same install/parser commands whenever that file changes. After pulling the
updated image, recreate only the application service and verify it:

```sh
docker compose up -d --no-deps --force-recreate adjutant
docker compose exec --user 10001:10001 adjutant cat /proc/self/attr/current
```

On AppArmor hosts, the last command should print `adjutant-sandbox (enforce)`. Then run the command
sandbox check below. Loading the profile does not disable Ubuntu's global namespace restrictions.

## Check the command sandbox

After installing or updating the image, verify command execution separately from Codex login:

```sh
docker compose run --rm --no-deps adjutant \
  node /usr/local/lib/adjutant/check-codex-sandbox.mjs
```

The check runs as Adjutant's UID 10001. It uses an isolated temporary Codex home, a file fixture,
and its own loopback listener. It makes no model request and reads no production data or login
state. Success means sandboxed reads and diagnostic utilities work, file writes are rejected, and
the sandbox cannot connect to the listener that is reachable outside it. Temporary files are
removed afterward. Run this check on the deployment host; a successful image build or Codex login
does not prove that the host permits the Linux sandbox to start.

The image includes the distribution's `bubblewrap` package. A missing-helper warning in an older
image means Codex is trying its bundled helper. A later `No permissions to create a new namespace`
error is a separate blocker: Docker seccomp or AppArmor can deny namespace creation even when
`kernel.unprivileged_userns_clone` is `1`. Database MCP calls can still succeed in that state, so a
completed run may have been unable to inspect local source files. Keep Codex's read-only and
network-disabled command sandbox enabled; the parent container has private runtime volumes and
shares the Tailscale network. See the [Codex sandbox prerequisites](https://developers.openai.com/codex/concepts/sandboxing#prerequisites).

## Codex logs and conversation failures

Follow application and Codex lifecycle/error logs with:

```sh
docker compose logs --follow --tail=100 adjutant
```

Conversation logs include the Discord `message_id`, decision, and elapsed time. A failure before
an investigation starts is logged here even though no inspector run exists. Investigation logs
carry a `run_id`; the inspector retains detailed events, messages, commands, and tool output.
Codex JSON error events are also logged, including failures that do not appear on stderr.

For more detail, set this in `adjutant.env` and recreate the service:

```dotenv
RUST_LOG=adjutant=info,adjutant::codex=debug
```

```sh
docker compose up -d --no-deps --force-recreate adjutant
```

Debug logging adds Codex stderr and item event types. It does not mirror model messages, reasoning
summaries, commands, or tool arguments/results into Docker logs. CLI error text and stderr may
still contain diagnostic context, so review those logs before sharing them. Output logging is
capped per invocation stream at 40 entries and 16 KiB of text, with at most 2 KiB per entry;
normal completion/failure summaries remain available after that cap. Restore `RUST_LOG=adjutant=info`
and recreate the service when finished. Configure Docker daemon log rotation separately on the VM;
these per-invocation caps do not limit total log growth over the lifetime of a container.

## Updating

When only the application images changed:

```sh
docker compose pull --policy always
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
fill in its required values before running `docker compose pull --policy always` or `docker compose up`.
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
