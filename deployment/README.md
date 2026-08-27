# Adjutant VM bundle

This directory is the complete runtime deployment bundle. Copy it to a Linux VM and pull the two
application images from GHCR; the Rust source tree and Dockerfile are not required on the VM.

The bundle intentionally does not contain secrets, persistent state, container images, or the
ShieldBattery source checkout. Compose uses the fixed project name `adjutant`, so replacing or
moving this directory continues to use the existing `adjutant-data`, `codex-home`, and
`tailscale-state` volumes.

## First installation

Install Docker Engine with the Compose plugin on a Linux x86-64 VM, make `/dev/net/tun` available,
and place a ShieldBattery checkout on the VM. From the copied directory:

```sh
cp .env.example .env
cp adjutant.env.example adjutant.env
cp mcp.env.example mcp.env
cp tailscale.env.example tailscale.env
chmod 600 .env adjutant.env mcp.env tailscale.env
```

Set the two `ghcr.io` image names and the absolute ShieldBattery checkout path in `.env`, then fill
the three service env files. A missing ShieldBattery path fails startup rather than creating an
empty directory.

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
test -d "$(sed -n 's/^SHIELDBATTERY_HOST_SOURCE_PATH=//p' .env)"
docker compose config --quiet
docker compose up -d tailscale
docker compose logs --tail=100 tailscale
docker compose up -d adjutant-mcp
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
`.env`, `adjutant.env`, `mcp.env`, or `tailscale.env`, then run the same update commands. For
example, from a source checkout (review changed `.example` files for new settings afterward):

```sh
rsync -av --delete \
  --exclude='.env' \
  --exclude='adjutant.env' \
  --exclude='mcp.env' \
  --exclude='tailscale.env' \
  deployment/ <vm>:/opt/adjutant/
```

The workflow publishes `latest`, branch, semantic-version, and `sha-*` tags. For a controlled
deployment or rollback, set both image values in `.env` to the same `sha-*` revision or to exact
`@sha256:` digests, then repeat the pull/up commands.

The full provisioning, security, database-role, and operations notes remain in
`docs/deployment.md` and `docs/database-mcp.md` in the source repository.
