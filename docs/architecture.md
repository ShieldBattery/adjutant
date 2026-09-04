# Architecture

Adjutant has five trust zones:

1. Discord supplies untrusted requests and evidence.
2. The orchestrator downloads and unpacks evidence under hard resource limits, stores an audit
   record, and starts Codex without passing service secrets into the child environment.
3. Codex gets read-only source/evidence access plus a dedicated configuration containing only
   read-only production MCPs. Its JSONL event stream and final answer are persisted before the
   answer is delivered back to Discord.
4. Credential-isolated sidecars hold the production database URL and Datadog service token. The
   database MCP accepts bounded read-only requests against curated views; the Datadog proxy is
   configured to forward only to the selected Datadog managed MCP and authenticates as a read-only
   service account.
5. The source synchronizer has ordinary public GitHub egress, constructs requests only for the
   fixed GitHub API/repository hosts, receives no Tailnet namespace or credentials, and has sole
   write access to the persistent source volume. Adjutant mounts only atomically published
   repository generations from that volume, read-only.

On Linux, Adjutant marks its own process non-dumpable before reading configuration. Combined
with the cleared/allowlisted child environment and dropped container capabilities, this prevents a
same-UID Codex shell command from recovering the bot's original secrets through `/proc`. Codex is
placed in its own process group so timeout/cancellation also kills descendant commands.

The Compose init shim runs as root, then the image entrypoint immediately drops Adjutant to UID/GID
10001. This makes the init shim's inherited environment unreadable to Codex; Adjutant's own
environment is protected by the non-dumpable setting. The entrypoint receives only `SETUID` and
`SETGID` for this one-way handoff, and `no-new-privileges` prevents the service from regaining them.

A separate Tailscale container owns the shared network namespace, resolver file, and `/dev/net/tun`.
Adjutant, the database MCP, and the Datadog proxy get Tailnet connectivity and MagicDNS through that
namespace but do not receive the Tailscale auth key, state, LocalAPI socket, capabilities, or PID
namespace. The inspector and database MCP listen only on shared loopback; Tailscale Serve is the
sole ingress path and terminates private HTTPS on ports 443 and 8443 respectively. The Datadog proxy
also listens on loopback but is deliberately absent from Tailscale Serve.

The Codex child necessarily shares Adjutant's network namespace, but `--sandbox read-only` denies
network access to every model-generated command. Codex reaches production data only through its
explicitly configured MCP transports; it receives neither the database URL nor the Datadog token.
The Rust parent performs evidence downloads before starting Codex and exposes only the resulting
local files to it. Do not replace this sandbox with a network-enabled permission profile: Tailscale
authenticates at the node boundary, so doing so would also grant the agent direct access to every
destination allowed to `tag:adjutant`.

Source synchronization is deliberately outside that namespace. It discovers bounded public
repositories from the configured GitHub organization, updates private bare mirrors, materializes a
complete commit-addressed generation of independent shallow clones, then atomically advances a
`current` symlink. A diagnosis that has already entered its working directory continues using the
old generation while later runs see the new one. Old generations outlive the maximum job duration
before cleanup, so the updater never mutates or removes source beneath an active Codex process.

The long-running process owns a bounded queue. Gateway handlers only validate and enqueue work;
they never download evidence or wait for Codex. A semaphore caps active diagnoses. Per-job
temporary directories are removed after completion, so user logs and dumps do not become part of
the persistent UI history. The database retains the request, evidence manifest, Codex event stream,
final report, and errors for the configured retention period.

The event stream has per-line, per-run byte, and per-run count limits. When one is reached,
Adjutant drains the remaining child output to avoid deadlock but persists one explicit
`adjutant.events_truncated` marker. Queue shutdown fails work that has not started and allows only
already-running jobs to finish under the end-to-end deadline.

## Data flow

```text
bug-report webhook --------\
                            +-> validate/filter -> bounded queue -> evidence workspace
staff request + evidence --/                                      |
                                                                   +-> ShieldBattery internal API
                                                                   +-> guarded ZIP extraction
                                                                      |
                                                                      v
Discord output <- final report <- Codex JSONL runner <-> SQLite -> inspection UI
                                       |
                                       +-> read-only source generation <- public GitHub synchronizer
                                       +-> database MCP -> curated PostgreSQL views
                                       +-> credential proxy -> Datadog managed MCP
```

## Inspectability

Every run is assigned a time-ordered UUID and moves through `queued`, `running`, `succeeded`, or
`failed`. The runner uses `codex exec --json --ephemeral --sandbox read-only`; each valid JSONL line
is appended to SQLite before the final report is marked complete. This exposes all reasoning
summaries, commands, file changes attempted, MCP calls, web searches, plan events, and errors that
Codex itself emits. It does not expose private hidden chain-of-thought that the Codex interface does
not return.

The web UI is server-rendered and read-only. It requires HTTP Basic authentication and must be
served only through Tailscale HTTPS or another TLS-terminating private proxy. `/healthz` contains no
run data and is the only unauthenticated route.

## Production tool policy

Adjutant never gives a database password or Datadog service token to the Codex child. The checked-in
Codex configuration allowlists the isolated database MCP's five reviewed read-only tools and a
reviewed subset of Datadog's read-only tools. Their credentials belong only to separate sidecars,
and both production identities lack write privileges.
`CODEX_ENV_PASSTHROUGH` is an explicit allowlist; Adjutant rejects attempts to pass its Discord or
UI credentials into Codex.
