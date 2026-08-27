# Adjutant

Adjutant is a private Discord bot that gives ShieldBattery staff a diagnostic agent where they
already triage reports. It watches the existing bug-report alert channel, retrieves the report's
private log bundle, asks Codex to correlate that evidence with the ShieldBattery source tree and
read-only production tools, and posts the diagnosis in a staff-only output channel. Staff can also
send one-off questions and attach ZIP files in the request channel.

The repository is a Rust 2024 Cargo workspace containing the bot and a separate read-only database
MCP. Each run is persisted to SQLite and can be inspected in a private, read-only web UI, including
the evidence manifest, Codex JSONL events, tool calls, reasoning summaries, final report, and
errors.

## What is implemented

- Alerts from the exact configured ShieldBattery webhook and public origin trigger bug-report
  diagnoses.
- Staff messages and Discord attachments trigger one-off diagnoses.
- Queue depth, concurrency, download size, aggregate ZIP expansion/file count, event history, process
  count, and end-to-end runtime are bounded.
- Discord and inspection-UI secrets never enter the Codex child environment.
- Codex runs with `codex exec --ephemeral --json --sandbox read-only` against a read-only source
  mount and approved read-only MCPs. Its model-generated commands have no network access, even
  though the parent service shares the sidecar's Tailnet connection.
- A Tailscale sidecar gives the service private ShieldBattery egress and exposes the inspector with
  Tailnet-only HTTPS; no application port is published on the Docker host.
- A credential-isolated Rust MCP gives Codex purpose-built user/game diagnostics plus bounded
  read-only PostgreSQL queries. Compose deploys it by default, and Tailscale Serve can also expose
  it privately to approved developers.
- A loopback-only credential proxy connects Codex to Datadog's managed MCP with a dedicated
  read-only service identity; neither Codex nor model-generated commands receive its token.
- `deployment/` is a copyable, image-only VM bundle, and the GitHub Actions workflow publishes the
  bot and MCP Dockerfile targets as separate GHCR images.
- Results are posted inline when possible and attached as `diagnosis.md` when too long for Discord.
- The container includes Mozilla's Rust `minidump-stackwalk` utility for Windows crash dumps.
- Startup recovery marks interrupted runs failed, and old run history is pruned automatically.

See [architecture](docs/architecture.md), the [deployment runbook](docs/deployment.md), the
[copyable VM bundle](deployment/README.md), the [database MCP guide](docs/database-mcp.md), the
[Datadog MCP guide](docs/datadog-mcp.md), and the
[ShieldBattery developer handoff](docs/shieldbattery-internal-api.md).

## Local verification

The current toolchain requirement is Rust 1.98 or newer.

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
whose service account lacks write permissions. Evidence is size-limited, extracted without trusting
ZIP paths, and removed with the per-job temporary directory.

This is a diagnostic system, not a remediation system. It does not edit ShieldBattery, write to
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
