# Working on Adjutant

Adjutant is a diagnostic system for ShieldBattery, not a remediation agent. Preserve the separation
between untrusted reports, the Codex runner, production credentials, and production systems.

## Project decisions

- Treat `../shieldbattery` as a read-only reference. Do not change ShieldBattery from this repo
  unless the operator explicitly reverses this decision. Describe required application-side work in
  `docs/shieldbattery-internal-api.md` or another handoff document instead.
- Tailscale is the application authorization boundary for ShieldBattery's internal report API and
  the developer-facing MCP endpoint. Do not reintroduce an internal API token or add an MCP bearer
  token without an explicit design change.
- The normal Compose configuration exposes the database MCP to authorized Tailnet developers on
  HTTPS port 8443. `deployment/tailscale/serve-agent-only.json` is the opt-out configuration.
- The inspection UI may show events, reasoning summaries, commands, and tool calls emitted by
  Codex. Never claim that it records private hidden chain-of-thought.
- This deployment is intended for a dedicated VM. Do not add Compose CPU or memory limits unless
  the operator requests them.

## Security invariants

- Discord text, ZIP contents, logs, dumps, database values, and MCP arguments are untrusted input.
  Preserve all download, archive-expansion, file-count, event-stream, process, queue, and time
  bounds when changing their paths through the system.
- The Codex child must keep a read-only filesystem sandbox and network-disabled model-generated
  commands. Do not pass Discord, UI, Tailscale, database, or other service credentials into its
  environment.
- Only `adjutant-mcp` receives the production database URL. It must bind to loopback, use bounded
  read-only transactions, retain SQL validation, and operate through a database role limited to
  curated non-sensitive views. Query telemetry must not contain raw SQL or result data.
- Tailscale Serve is the only ingress for the inspector and remote MCP. Do not publish their
  container ports, enable Funnel, or treat forwarded Tailscale identity headers as an independent
  authentication mechanism.
- `deployment/config/codex.toml` is reviewable tool policy. Codex login state and `auth.json`
  belong only in the private `codex-home` volume.

## Workspace and deployment layout

- The repository is a Rust 2024 workspace requiring Rust 1.98 or newer. The root package is the
  Discord/orchestration service; `crates/adjutant-mcp` is the isolated PostgreSQL MCP server.
- Keep Rust dependencies current and commit `Cargo.lock`. Review deliberately pinned container and
  CLI versions before updating them rather than changing them incidentally.
- The Dockerfile's last stage is the MCP runtime. Compose must continue to select `runtime`
  explicitly for `adjutant` and `mcp-runtime` for `adjutant-mcp` in the source-build overlay.
- `deployment/` is a self-contained, image-only VM bundle. Keep production build contexts out of
  `deployment/compose.yaml`; local builds belong in the repository-only `compose.build.yaml`.
- Keep the top-level Compose project name `adjutant` so moving the deployment directory does not
  orphan its persistent SQLite, Codex login, or Tailscale identity volumes.
- Adjutant, the database MCP, and the Datadog credential proxy use
  `network_mode: service:tailscale`; they intentionally share networking, but not the Tailscale
  state volume, LocalAPI socket, auth key, capabilities, or PID namespace.
- Keep examples free of real guild/channel IDs, Tailnet names, hostnames, credentials, report data,
  and database contents. Never commit deployment `.env`, `adjutant.env`, `datadog-mcp.env`,
  `mcp.env`, `tailscale.env`, Codex auth state, or the runtime SQLite database.

## Verification

Run these for Rust changes:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

For Compose, Dockerfile, environment, or Tailscale changes, also run:

```sh
docker compose --env-file deployment/.env.example \
  -f deployment/compose.yaml -f compose.validate.yaml \
  config --no-env-resolution --quiet
docker compose --env-file deployment/.env.example \
  -f deployment/compose.yaml -f compose.validate.yaml -f compose.build.yaml \
  build adjutant-mcp adjutant
```

Changes to MCP transport or query behavior should additionally be smoke-tested against a temporary
PostgreSQL instance through an actual MCP initialize/list/call session.

Keep commits focused around completed, verified checkpoints. Preserve unrelated working-tree
changes, never commit secrets or production evidence, and do not rewrite existing history without
an explicit request.
