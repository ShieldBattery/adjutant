# Adjutant

Adjutant is a private Discord bot that gives ShieldBattery staff a diagnostic agent where they
already triage reports. It watches the existing bug-report alert channel, retrieves the report's
private log bundle, asks Codex to correlate that evidence with the ShieldBattery source tree and
read-only production tools, and posts the diagnosis in a staff-only output channel. Staff can also
send one-off questions and attach ZIP files in the request channel.

The bot is a Rust 2024 service packaged as a hardened Docker container. Each run is persisted to
SQLite and can be inspected in a private, read-only web UI, including the evidence manifest, Codex
JSONL events, tool calls, reasoning summaries, final report, and errors. Hidden chain-of-thought is
not available from Codex and is not claimed to be captured.

## What is implemented

- Alerts from the exact configured ShieldBattery webhook and public origin trigger bug-report
  diagnoses.
- Staff messages and Discord attachments trigger one-off diagnoses.
- Queue depth, concurrency, download size, aggregate ZIP expansion/file count, event history, process
  count, memory, CPU, and end-to-end runtime are bounded.
- ShieldBattery service credentials never enter the Codex child environment.
- Codex runs with `codex exec --ephemeral --json --sandbox read-only` against a read-only source
  mount and optional read-only MCPs.
- Results are posted inline when possible and attached as `diagnosis.md` when too long for Discord.
- The container includes Mozilla's Rust `minidump-stackwalk` utility for Windows crash dumps.
- Startup recovery marks interrupted runs failed, and old run history is pruned automatically.

See [architecture](docs/architecture.md), the [deployment runbook](docs/deployment.md), and the
[ShieldBattery developer handoff](docs/shieldbattery-internal-api.md).

## Local verification

The current toolchain requirement is Rust 1.98 or newer.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
```

## Intended trust boundary

Adjutant treats Discord text, uploaded files, log contents, and crash dumps as untrusted evidence.
Codex runs with a read-only filesystem sandbox and no approval flow. The child process receives an
explicit environment allowlist, and its dedicated Codex configuration must expose only read-only
MCP tools. Evidence is size-limited, extracted without trusting ZIP paths, and removed with the
per-job temporary directory.

This is a diagnostic system, not a remediation system. It does not edit ShieldBattery, write to
production data, or send messages anywhere except the configured Discord output channel.
