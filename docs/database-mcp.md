# Read-only database MCP

`adjutant-mcp` is a separate Rust process and container. It is the only Adjutant component that
receives a production database URL. Codex reaches it over the network namespace's loopback
interface, so the model never receives database credentials and model-generated shell commands
remain network-disabled.

The normal Compose deployment also publishes this MCP to the Tailnet through Tailscale Serve:

- Adjutant Codex sessions: `http://127.0.0.1:8081/mcp`
- Tailnet developer clients: `https://<adjutant-node>.<tailnet>.ts.net:8443/mcp`
- Local health check: `http://127.0.0.1:8081/healthz`

There is no Docker host port and Funnel is explicitly disabled. Tailscale ACL grants are the
authorization boundary for port 8443. To keep the MCP agent-local, set
`TAILSCALE_SERVE_CONFIG=serve-agent-only.json` in the copied deployment bundle's `.env` and restart
the Tailscale service.

## Tools and bounds

The server exposes five read-only tools. Prefer the purpose-built tools for their matching
investigations; they use fixed parameterized queries and return stable diagnostic objects:

- `search_users` performs a bounded case-insensitive exact/prefix display-name search and returns
  only safe public identity fields.
- `get_user_diagnostics` resolves one user by ID or exact display name and returns their public
  identity, aggregate race statistics, current matchmaking ratings, and a bounded recent-game
  summary.
- `get_game_diagnostics` composes the game record, participants and result reports, netcode-v2
  placement/relay history, desync events, matchmaker formation inputs, and rating changes for one
  game ID.
- `database_schema` discovers the tables, views, and columns visible to its PostgreSQL role. It
  supports optional schema and relation filters.
- `query_database` is the escape hatch for one PostgreSQL `SELECT`, `WITH`, `VALUES`, or `TABLE`
  query and returns JSON rows plus explicit truncation metadata.

The generic query parser rejects multiple statements/terminators, DDL/DML (including
data-modifying CTEs), bind placeholders, row-locking clauses, and advisory-lock functions. The
purpose-built tools never accept SQL and bind every lookup value as a PostgreSQL parameter. All
execution takes place in a transaction which issues `SET TRANSACTION READ ONLY`, a short statement
timeout, and a short lock timeout. The service also caps connection count, SQL length, returned
rows, per-row size, component list lengths, and aggregate response bytes.

Those controls reduce mistakes and resource abuse, but PostgreSQL privileges are the hard data
boundary. Do not give the login direct table access or rely on prompt instructions to hide fields.

## Recommended PostgreSQL roles and views

Use a non-login view owner and a separate login role. The view owner gets narrowly selected source
table privileges; the login gets only access to the curated views. Run the setup as a privileged
database administrator that can create roles, grant privileges, and `SET ROLE` to the non-login
view owner. Adapt names and columns to the deployed ShieldBattery schema, and keep this SQL in
production database administration rather than the ShieldBattery application migration sequence.

```sql
CREATE ROLE adjutant_view_owner
  NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS;

CREATE ROLE adjutant_diagnostics
  LOGIN PASSWORD '<generate-and-store-a-long-random-password>'
  NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS
  CONNECTION LIMIT 4;

ALTER ROLE adjutant_diagnostics IN DATABASE shieldbattery
  SET default_transaction_read_only = on;
ALTER ROLE adjutant_diagnostics IN DATABASE shieldbattery
  SET statement_timeout = '5s';
ALTER ROLE adjutant_diagnostics IN DATABASE shieldbattery
  SET lock_timeout = '1s';
ALTER ROLE adjutant_diagnostics IN DATABASE shieldbattery
  SET search_path = adjutant_diagnostics, pg_catalog;

CREATE SCHEMA adjutant_diagnostics AUTHORIZATION adjutant_view_owner;
REVOKE ALL ON SCHEMA adjutant_diagnostics FROM PUBLIC;
GRANT USAGE ON SCHEMA adjutant_diagnostics TO adjutant_diagnostics;
GRANT CONNECT ON DATABASE shieldbattery TO adjutant_diagnostics;

-- Grant the non-login owner only the source relations needed by the curated views. It needs
-- schema USAGE to resolve those exact relations, but receives no blanket table privileges.
GRANT USAGE ON SCHEMA public TO adjutant_view_owner;
GRANT SELECT ON public.users, public.user_stats, public.games, public.games_users,
                public.uploaded_maps, public.game_desync_events,
                public.matchmaking_ratings, public.matchmaking_seasons,
                public.matchmaking_match_formations, public.matchmaking_rating_changes
  TO adjutant_view_owner;

-- Creating the views as this narrowly privileged owner is what prevents their security-definer
-- access from inheriting the database administrator's broader privileges.
SET ROLE adjutant_view_owner;

CREATE VIEW adjutant_diagnostics.users
  WITH (security_barrier = true)
AS
SELECT id, name::text AS name, created
FROM public.users;

CREATE VIEW adjutant_diagnostics.games
  WITH (security_barrier = true)
AS
SELECT id, start_time, map_id, config, game_length, results,
       disputable, dispute_requested, dispute_reviewed,
       selected_matchup, assigned_matchup, manually_resolved_at,
       netcode_v2_session::text AS netcode_v2_session,
       netcode_v2_relays, netcode_v2_requested_regions
FROM public.games;

CREATE VIEW adjutant_diagnostics.game_users
  WITH (security_barrier = true)
AS
SELECT game_id, user_id, start_time,
       selected_race::text AS selected_race,
       assigned_race::text AS assigned_race,
       reported_results, result::text AS result, apm,
       reported_at, replay_file_id, team,
       departure_kind, departure_time, relay_report_time, relay_report_frame
FROM public.games_users;

CREATE VIEW adjutant_diagnostics.maps
  WITH (security_barrier = true)
AS
SELECT id, name, encode(map_hash, 'hex') AS map_hash
FROM public.uploaded_maps;

CREATE VIEW adjutant_diagnostics.user_stats
  WITH (security_barrier = true)
AS
SELECT user_id,
       p_wins, p_losses, t_wins, t_losses, z_wins, z_losses,
       r_wins, r_losses,
       r_p_wins, r_p_losses, r_t_wins, r_t_losses, r_z_wins, r_z_losses
FROM public.user_stats;

CREATE VIEW adjutant_diagnostics.current_matchmaking_ratings
  WITH (security_barrier = true)
AS
WITH current_season AS (
  SELECT id, name, start_date
  FROM public.matchmaking_seasons
  WHERE start_date <= CURRENT_TIMESTAMP AT TIME ZONE 'UTC'
  ORDER BY start_date DESC
  LIMIT 1
)
SELECT r.user_id, r.matchmaking_type::text AS matchmaking_type,
       s.id AS season_id, s.name AS season_name, s.start_date AS season_start_date,
       r.rating, r.uncertainty, r.volatility, r.points, r.points_converged,
       r.bonus_used, r.num_games_played, r.lifetime_games,
       r.wins, r.losses, r.last_played_date
FROM public.matchmaking_ratings AS r
JOIN current_season AS s ON s.id = r.season_id;

CREATE VIEW adjutant_diagnostics.game_desync_events
  WITH (security_barrier = true)
AS
SELECT game_id, sync_ordinal, detected_at, game_frame, no_majority,
       diverged_user_ids, received_at
FROM public.game_desync_events;

CREATE VIEW adjutant_diagnostics.matchmaking_formations
  WITH (security_barrier = true)
AS
SELECT game_id, matchmaking_type::text AS matchmaking_type,
       quality, skill_variance, win_probability,
       team_a_rating, team_b_rating, max_latency, created_at, id, fail_phase
FROM public.matchmaking_match_formations;

CREATE VIEW adjutant_diagnostics.matchmaking_rating_changes
  WITH (security_barrier = true)
AS
SELECT game_id, user_id, matchmaking_type::text AS matchmaking_type,
       change_date, outcome::text AS outcome,
       rating, rating_change, uncertainty, uncertainty_change,
       volatility, volatility_change, points, points_change,
       probability, lifetime_games
FROM public.matchmaking_rating_changes;

RESET ROLE;

GRANT SELECT ON ALL TABLES IN SCHEMA adjutant_diagnostics TO adjutant_diagnostics;
```

Then apply [`deployment/sql/moderation-views.sql`](../deployment/sql/moderation-views.sql)
as the database administrator to add the moderation and account-linking views described below.
Use the same step for an existing installation; do not rerun the role-creation SQL. The script
requires the ShieldBattery migration `20260913120000_add_unban_columns_to_user_bans.sql` and the
earlier matchmaking/restriction tables, including queue-time rating (`20260625120000`) and
failed formations (`20260628120000`). Check the deployed schema first: a source checkout alone
does not establish that these migrations have reached production. The script is transactional and
can be reapplied.

Add explicit views for further diagnostic data only when an investigation needs it repeatedly.
Name every projected column. In particular, never expose
`users_private`, password/auth/session/token material, email addresses, signup/login IPs, payment
data, per-player game `result_code` values, or unrestricted free-form administrative tables. The
game result code is a request-authentication secret despite its innocuous name. Review each new
view exactly as you would a new production API response.

Also audit function privileges as a deployment prerequisite. Curated views do not constrain the
functions callable from a `SELECT`: PostgreSQL commonly grants `EXECUTE` on functions to `PUBLIC`,
and a `SECURITY DEFINER` or extension function can expose data or external effects that ordinary
table grants do not. Revoke broad execution from any such functions, explicitly grant only the
safe functions each application role needs, and do not make `adjutant_diagnostics` a member of an
application or monitoring role.

The example is intentionally not a checked-in ShieldBattery migration: the ShieldBattery app does
not need to know the login password or own Adjutant's operational access. A database administrator
should create and rotate the role independently.

## Moderation and linked-account evidence

These views are available through the existing `database_schema` and `query_database` tools once
the SQL is applied. No image rebuild, MCP restart, or tool-policy change is required.
`get_user_diagnostics` continues to return the identity/statistics/game summary; use the views for
moderation investigations.

| View in `adjutant_diagnostics` | Contents |
| --- | --- |
| `matchmaking_completions` | Per-user queue outcomes (`found`, `cancel`, `disconnect`), search duration, completion time, and queue-time rating. |
| `matchmaking_formations` | Launched and failed match formations, decision inputs, and failure phase; failed rows have no game ID. |
| `user_bans` | Account ban history, start/end times, banning staff ID, and early-unban staff ID/time. |
| `user_restrictions` | Privilege restriction history, including chat, reporting, matchmaking, and avatar upload; kind, times, staff ID, and recognized reason code. |
| `matchmaking_bans` | Automatic ban records with level, expiry, escalation-decay time, triggering account, and associated account IDs. |
| `user_identifier_bans` | Identifier-ban times, original account, identifier type, and associated account IDs. |
| `user_identifier_restrictions` | Identifier-based privilege restrictions with associated accounts, original account, staff ID, times, and recognized reason code. |
| `user_identifiers` | Per-account/type metadata: number of stored identifiers, earliest/latest observation, and total times seen. |
| `user_identifier_matches` | Account pairs sharing identifiers, grouped by identifier type, with matching identifier count and observation timestamps for each account. |

The login receives only these views. Stored identifier hashes remain inside the non-login view
owner's joins. Free-form staff notes, ban reasons, and unban reasons are omitted. Restriction
reasons are projected only when they match the reviewed code list for that kind in the SQL; new
codes return `NULL` until reviewed. A `NULL` reason can therefore mean absent or unrecognized.

`user_identifiers` is an aggregate, not a copy of the underlying table. Use
`user_identifier_matches` to find account links. Both exclude identifier type `0`, following
ShieldBattery's linking logic. Matching multiple hashes of a single type counts as one matching
type. The current production source uses a four-type threshold (development uses one); verify
the relevant deployed version before treating that threshold as enforcement policy. Shared
identifiers are evidence of a shared machine, not proof of the same person. Observation ranges
are historical summaries and do not establish simultaneous use.

For example, find candidates for a synthetic account ID, ranked by distinct matching types:

```sql
SELECT linked_user_id, count(DISTINCT identifier_type) AS matching_types,
       min(linked_first_used) AS first_observed, max(linked_last_used) AS last_observed
FROM adjutant_diagnostics.user_identifier_matches
WHERE user_id = 123
GROUP BY linked_user_id
HAVING count(DISTINCT identifier_type) >= 4
ORDER BY matching_types DESC, linked_user_id
LIMIT 20
```

Always filter link queries by `user_id`; the view joins historical identifiers and should not be
used to enumerate the entire account graph. The ordinary statement timeout and result bounds
still apply. For per-type evidence, select the rows for both `user_id` and `linked_user_id`.

In identifier punishment views, `user_id` means an account with a matching nonzero identifier.
It is not necessarily the account that caused the punishment. `first_user_id` (identifier bans
and restrictions) and `triggered_by` (automatic matchmaking bans) preserve that distinction.
The views retain punishments without a remaining association as rows with `user_id = NULL`.
Identifier bans/restrictions exclude type `0`; matchmaking bans retain type `0` records for
their direct `triggered_by` path but never associate accounts using that type. One punishment
can appear for multiple associated accounts. Deduplicate matchmaking bans by `id` when querying
the direct trigger path:

```sql
SELECT DISTINCT id, identifier_type, triggered_by, ban_level,
       created_at, expires_at, clears_at, cleared
FROM adjutant_diagnostics.matchmaking_bans
WHERE user_id = 123 OR triggered_by = 123
ORDER BY created_at DESC, id
LIMIT 50
```

This returns evidence, not an effective-ban verdict. Automatic matchmaking enforcement has a
direct-trigger path and a threshold-based identifier path grouped by ban level and expiry.
Privilege restrictions and account bans use their own checks. Do not equate one matching
identifier with an applicable punishment.

For automatic matchmaking bans, `expires_at` ends queue exclusion; `clears_at` ends the ban's
effect on later escalation. `cleared` is periodically updated bookkeeping and can lag behind
`clears_at`. Keep these distinct when diagnosing a user who can queue again but still has an
elevated ban level. Identifier restrictions are upserted per identifier/kind, and identifier
bans can be extended or lifted in place. These views retain records that the application has
not cleaned up; they are not an immutable moderation audit log.

Account-ban and identifier observation/ban timestamps use ShieldBattery's legacy UTC
`timestamp without time zone` representation. Restriction and automatic matchmaking timestamps
are `timestamptz`. Compare legacy times with `CURRENT_TIMESTAMP AT TIME ZONE 'UTC'`, for example:

```sql
SELECT id, user_id, start_time, end_time, banned_by, unbanned_by, unbanned_at
FROM adjutant_diagnostics.user_bans
WHERE user_id = 123
  AND start_time <= CURRENT_TIMESTAMP AT TIME ZONE 'UTC'
  AND end_time > CURRENT_TIMESTAMP AT TIME ZONE 'UTC'
ORDER BY start_time DESC, id
LIMIT 20
```

An early unban moves `end_time` to the lift time; `unbanned_at` and `unbanned_by` preserve the
explicit lift attribution.

### Matchmaking outcome logs

`matchmaking_completions` records per-user queue completions. `completion_time` is a legacy UTC
timestamp; `search_time_millis` is the recorded search duration, and `rating` is nullable for
historical records. A `found` outcome does not establish that a game launched. These records
have no game or formation ID, so a temporal correlation is not a definitive join.

```sql
SELECT id, matchmaking_type, completion_type, search_time_millis, completion_time, rating
FROM adjutant_diagnostics.matchmaking_completions
WHERE user_id = 123
  AND completion_time >= (CURRENT_TIMESTAMP AT TIME ZONE 'UTC') - INTERVAL '7 days'
ORDER BY completion_time DESC, id
LIMIT 50
```

`matchmaking_formations` now includes failed starts as well as launched games. A launched
formation has a `game_id` and no `fail_phase`; a failed formation has no `game_id` and a phase
of `accepting`, `drafting`, or `loading`. The formation `id` uniquely identifies each row,
including failures that have no game ID. `created_at` is timezone-aware. This is best-effort telemetry: missing rows do not establish
that no attempt occurred. Existing `get_game_diagnostics` lookups still select by game ID.

```sql
SELECT id, matchmaking_type, fail_phase, quality, max_latency, created_at
FROM adjutant_diagnostics.matchmaking_formations
WHERE game_id IS NULL
  AND created_at >= CURRENT_TIMESTAMP - INTERVAL '1 day'
ORDER BY created_at DESC, id
LIMIT 50
```

## Local verification

Build the MCP binary, then run the synthetic integration check from the repository root:

```sh
cargo build -p adjutant-mcp
python scripts/test-moderation-mcp.py
```

The check requires Python 3.10 or newer and Docker. It starts a temporary PostgreSQL container and a local MCP
process, provisions the views twice, and exercises MCP initialization, tool discovery, and queries
as the restricted role. It checks account matching, punishment associations, redaction, grants,
and result bounds. It uses synthetic data and removes its own temporary resources afterward.

## Adjutant configuration

Copy `deployment/mcp.env.example` to `deployment/mcp.env`, put the dedicated role's URL in
`ADJUTANT_MCP_DATABASE_URL`, and restrict the file to the deployment account. For Tailnet developer
access, also set `ADJUTANT_MCP_TAILSCALE_HOSTNAME` to the exact `*.ts.net` FQDN reported by
`tailscale serve status`; this is an allowlisted HTTP Host, not an additional credential. This file
is loaded only by the MCP container; neither the Discord bot nor Codex receives it.

The checked-in `deployment/config/codex.toml` makes this server required and allowlists its five
reviewed tools. Additional production MCPs, such as Datadog, should be added to that file with
equally narrow tool and approval policies. Keep credentials in the Codex volume or environment
references, never in Git.

For developer use, any Streamable HTTP MCP client can use the Tailnet HTTPS URL without a bearer
token. Grant port 8443 only to the developer/staff identities that should be able to query the
curated views. For example, a developer can add this to their Codex `config.toml`:

```toml
[mcp_servers.shieldbattery_database]
url = "https://<adjutant-node>.<tailnet>.ts.net:8443/mcp"
enabled_tools = [
  "search_users",
  "get_user_diagnostics",
  "get_game_diagnostics",
  "database_schema",
  "query_database",
]
default_tools_approval_mode = "writes"
```

Tailscale Serve attaches peer identity headers to proxied requests. The MCP records them in its
structured request logs for audit context but does not treat caller-controlled headers as
authorization; the Tailnet ACL remains the access check.
