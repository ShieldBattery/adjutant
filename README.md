# Adjutant

Adjutant is a private Discord bot that gives ShieldBattery staff a diagnostic agent where they
already triage reports. It watches the existing bug-report alert channel, retrieves the report's
private log bundle, asks Codex to correlate that evidence with the ShieldBattery source tree and
read-only production tools, and posts the diagnosis in a staff-only output channel. Staff can also
send one-off questions and attach ZIP files in the request channel.

This repository is greenfield and not yet deployable. The implementation and deployment runbook
are being built in the next commits.

## Intended trust boundary

Adjutant treats Discord text, uploaded files, log contents, and crash dumps as untrusted evidence.
Codex runs with a read-only filesystem sandbox and no approval flow. The child process receives an
explicit environment allowlist, and its dedicated Codex configuration must expose only read-only
MCP tools. Evidence is size-limited, extracted without trusting ZIP paths, and removed with the
per-job temporary directory.

