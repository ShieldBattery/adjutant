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
       team_a_rating, team_b_rating, max_latency, created_at
FROM public.matchmaking_match_formations
WHERE game_id IS NOT NULL;

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
