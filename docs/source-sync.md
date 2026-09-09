# Source synchronization

Production Adjutant does not require a checkout on the VM. The `source-sync` service uses GitHub's
public organization API to discover every non-empty public repository in
`SOURCE_SYNC_GITHUB_ORG`, then maintains bounded-depth Git mirrors and read-only working snapshots
in the persistent `source-repos` volume. It needs no GitHub credential.

The service runs from the normal Adjutant image but executes the Rust
`adjutant-source-sync` binary. It uses a dedicated ordinary Docker egress network rather than the
service/Tailscale container network, receives no service env file, and has no access to Discord,
database, Datadog, or Tailscale credentials. Adjutant mounts the volume read-only; `source-sync` is
its sole writer.

The binary constructs requests only to `api.github.com` and `github.com` and disables redirects.
Docker's ordinary network does not itself enforce an FQDN allowlist; operators that need hard
domain-only egress should add a VM firewall or outbound proxy policy.

## Consistent generations

Each refresh follows this sequence:

1. Enumerate a bounded number of repositories and fetch each default branch into its private,
   shallow bare mirror.
2. Resolve every repository to an exact commit.
3. Materialize independent shallow clones for all repositories together in a staging generation.
4. Write `.adjutant-source-manifest.json` with the requested history depth and the selected branch
   and commit for every repository.
5. Rename the complete staging directory and atomically advance the `current` symlink.

At run startup, Adjutant resolves `current/ShieldBattery` to its canonical generation path and
passes that fixed directory to Codex. Later commands and relative sibling-repository reads keep
using the same generation even when `current` advances. The prompt tells Codex to read the manifest
one directory above and cite materially relevant commits. Old generations are retained long enough
for in-flight runs before conservative cleanup.

The clones do not borrow objects from the mutable mirrors, so mirror pruning cannot invalidate a
published generation.

If GitHub is temporarily unavailable, the synchronizer logs the error and keeps the last complete
generation. On a first installation, Compose holds Adjutant until the initial generation is ready.
An incomplete refresh is never published. Publication is atomic during normal operation. After an
abrupt host or container failure, the health check rejects incomplete state and the next startup
removes abandoned staging directories before rebuilding.

## Selecting the relevant release

The manifest records fetched default-branch tips, not deployed releases. Agent guidance selects
source according to the question:

- For a bug report, use the version recorded in its logs or explicit build evidence, even when
  the report arrived through a staff request and concerns an older release.
- For a staff request without a version, assume the latest applicable release commit. ShieldBattery
  normally records releases as `Version X.Y.Z.` commit subjects; tags are not required or fetched.
- Inspect later commits separately to see whether a reported problem has already been fixed.
  Distinguish fixes included in later releases from changes after the latest release that may
  still be unreleased. Source history alone does not confirm what is deployed.

Codex inspects first-parent release candidates and their package metadata, then reads historical
files with `git show SHA:path`, searches with `git grep -n -e pattern SHA -- path`, and compares
relevant changes with `git log SHA..HEAD -- path` and `git diff SHA..HEAD -- path`. It does not check
out a release or create a worktree. Multiple runs can inspect different releases in the same
read-only generation without changing files or refs. Ordinary working-tree reads still show the
fetched tip, so version-specific conclusions must cite the selected commit.

Release selection is evidence-driven guidance, not a deterministic claim that any version-looking
commit was deployed. Historical subjects can repeat versions or include annotations. Client log
versions do not establish the deployed server version, and sibling repositories have independent
release histories. Ambiguity and assumptions must be stated in the answer.

History is bounded by `SOURCE_SYNC_GIT_DEPTH` (200 by default, at most 10,000). If an older version
or relevant ancestry is missing, Codex reports that limit instead of silently using HEAD. The
operator can increase the depth in `.env` and run `docker compose up -d --force-recreate source-sync`
to apply it. Changing depth publishes a new generation even if repository tips are unchanged.
Deeper history increases fetch and storage costs; retain the existing size, runtime, and
generation-retention bounds. Newly retained history is available to runs using a new published generation.

## Configuration

The tracked `deployment/.env.example` contains all settings:

- `SOURCE_SYNC_GITHUB_ORG`: public GitHub organization; defaults to `ShieldBattery`.
- `SOURCE_SYNC_INTERVAL_SECONDS`: delay between refreshes; defaults to 15 minutes.
- `SOURCE_SYNC_GIT_DEPTH`: recent commits retained per default branch; defaults to 200.
- `SOURCE_SYNC_RETENTION_SECONDS`: minimum age before an old generation may be removed; defaults to
  two hours. It must be at least `JOB_TIMEOUT_SECONDS` plus a synchronization interval.
- `SOURCE_SYNC_MAX_REPOSITORIES`: hard discovery bound; defaults to 100.
- `SOURCE_SYNC_MAX_REPOSITORY_KIB`: reject a repository whose GitHub API-reported size exceeds this
  preflight bound; defaults to 1 GiB.
- `SOURCE_SYNC_MAX_TOTAL_KIB`: reject an organization whose summed API-reported size exceeds this
  preflight bound; defaults to 4 GiB.
- `JOB_TIMEOUT_SECONDS`: shared maximum diagnostic runtime used to validate safe generation
  retention; defaults to 30 minutes.

`ShieldBattery` is the required primary repository and Codex working directory. GitHub's reported
sizes are early safety checks, not exact transfer or filesystem quotas. For a hard storage bound,
monitor or quota the `source-repos` Docker volume. Every Git subprocess also has a 15-minute
deadline and low-speed timeout.

New public repositories appear automatically on a later refresh, so adding one does not require an
Adjutant image or deployment update. Empty repositories are skipped. Repositories that disappear
from GitHub disappear only from newly published generations; historical generations remain until
normal retention cleanup.

## Operations

Inspect synchronization and the currently published commits:

```sh
docker compose ps source-sync
docker compose logs --tail=100 source-sync
docker compose exec source-sync /usr/local/bin/adjutant-source-sync healthcheck
docker compose exec source-sync \
  sed -n '1,160p' /workspace/repos/current/.adjutant-source-manifest.json
```

The periodic loop is normally sufficient. Restarting the service requests an immediate refresh:

```sh
docker compose restart source-sync
docker compose logs --tail=100 source-sync
```

The `source-repos` volume is reproducible from public GitHub and normally does not require backup.
Deleting it discards mirrors and history cache; the next startup performs a complete initial sync
before Adjutant becomes ready.
