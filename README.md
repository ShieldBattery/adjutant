# Adjutant

Adjutant is a private Discord bot that helps ShieldBattery staff investigate problems and answer
questions with read-only data. It watches the existing bug-report alert channel, retrieves the report's
private log bundle, asks Codex to correlate that evidence with the ShieldBattery source tree and
read-only production tools, and posts the diagnosis in a staff-only output channel. Staff can
also ask for data lookups, comparisons, and summaries, attach ZIP files, or include a game link to collect
that game's retained flight recordings, replays, and map.

The repository is a Rust 2024 Cargo workspace containing the bot and a separate read-only database
MCP. Each run is persisted to SQLite and can be inspected in a private, read-only web UI, including
the evidence manifest, Codex JSONL events, tool calls, reasoning summaries, final report, and
errors.

## What is implemented

- Alerts from the exact configured ShieldBattery webhook and public origin trigger bug-report
  diagnoses.
- Staff mentions and replies get acknowledged; Astra distinguishes conversation, status, and
  investigation requests from ordinary chatter in the two staff channels. A canonical ShieldBattery game
  link (or an explicitly labeled raw game UUID) also retrieves its available flight recordings,
  replays, map, and artifact metadata through the private internal API, up to configured limits.
- Bug-report links work inline in staff requests. Codex can also request a report or a game's
  available maps, replays, flight recordings, and artifact metadata by ID during an investigation.
  The host collects them through the internal API and updates the evidence manifest; repeated IDs
  reuse existing files and collection limits apply across the whole run.
- Queue depth, concurrency, generic download size, game artifact count/per-file/aggregate bytes,
  aggregate ZIP expansion/file count, event history, process count, and end-to-end runtime are bounded.
- The Discord credential and any legacy `ADJUTANT_UI_TOKEN` value never enter the Codex child
  environment.
- Diagnostic runs use an ephemeral Codex app-server thread over private stdio, with a read-only
  sandbox and approved read-only MCPs. Short conversation routing uses `codex exec`. Model-generated
  commands have no network access, even though the parent service shares the sidecar's Tailnet
  connection.
- A credential-free Rust source synchronizer discovers the organization's public GitHub
  repositories and atomically refreshes persistent, read-only snapshots without rebuilding or
  redeploying Adjutant.
- A Tailscale sidecar gives the service private ShieldBattery egress and exposes the inspector with
  Tailnet-only HTTPS on port 443; Tailnet ACLs and grants authorize access, with no separate browser
  password and no application port published on the Docker host.
- A credential-isolated Rust MCP gives Codex purpose-built user/game diagnostics plus bounded
  read-only PostgreSQL queries. Compose deploys it by default, and Tailscale Serve can also expose
  it privately to approved developers.
- A loopback-only credential proxy connects Codex to Datadog's managed MCP with a dedicated
  read-only service identity; neither Codex nor model-generated commands receive its token.
- `deployment/` is a copyable, image-only VM bundle, and the GitHub Actions workflow publishes the
  bot/source-sync and MCP Dockerfile targets as separate GHCR images.
- Native message replies can steer an active investigation with relevant text corrections or
  changes of focus. Attachments and updates that cannot enter the active run become ordered
  follow-up investigations. Substantive progress notes and status lookups keep staff informed.
  Bounded recent context can expand through paginated channel history, and
  searchable case notes preserve past findings and attributed corrections. See the
  [Discord interaction guide](docs/discord-interaction.md).
- Results lead with the answer or a short summary, followed by relevant supporting details.
  Next checks and diagnostic sections are optional. Longer results are attached as `result.md`;
  the inspector retains the full result.
- The container includes Mozilla's Rust `minidump-stackwalk` utility for Windows crash dumps.
- Startup recovery marks interrupted runs failed, and old run history is pruned automatically.

See [architecture](docs/architecture.md), the [deployment runbook](docs/deployment.md), the
[copyable VM bundle](deployment/README.md), the [database MCP guide](docs/database-mcp.md), the
[Datadog MCP guide](docs/datadog-mcp.md), the [source-sync guide](docs/source-sync.md), and the
deployment's tracked configuration examples.

## Local verification

The current toolchain requirement is Rust 1.98.1 or newer. [Rust CI](.github/workflows/rust.yml)
runs formatting, Clippy, and workspace tests on pull requests, pushes to `main`, and `v*` tags.
CI uses Rust 1.98.1 and the committed lockfile. Clippy warnings fail CI via `-D warnings`,
including the enabled `all` and `pedantic` groups. Deliberate lint allowances in the manifests
(such as `missing_errors_doc`) and narrowly scoped source attributes remain effective.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

## Intended trust boundary

Adjutant treats Discord text, uploaded files, log contents, and crash dumps as untrusted evidence.
Codex runs with a read-only filesystem sandbox and no approval flow. The child process receives an
explicit environment allowlist, and its checked-in Codex configuration exposes only approved
read-only MCP tools. Database credentials exist only in the MCP container and the database role can
select only curated non-sensitive views. The Datadog service token exists only in its proxy sidecar,
whose service account lacks write permissions. The source synchronizer has public GitHub egress but
no Tailnet connection or service credentials; Adjutant mounts its snapshot volume read-only.
Evidence is size-limited, game artifact paths are re-derived and integrity hashes are checked, ZIPs
are extracted without trusting archive paths, and everything is removed with the per-job temporary
directory.

This system performs read-only investigation and analysis, without remediation. It does not edit ShieldBattery, write to
production data, or send messages anywhere except the configured Discord output channel.

## License

Licensed under either of

- Apache License, Version 2.0
  ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license
  ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
