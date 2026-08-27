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
`TAILSCALE_SERVE_CONFIG=serve-agent-only.json` in `.env` and restart the Tailscale service.

## Tools and bounds

The server deliberately has only two tools:

- `database_schema` discovers the tables, views, and columns visible to its PostgreSQL role. It
  supports optional schema and relation filters.
- `query_database` executes one PostgreSQL `SELECT`, `WITH`, `VALUES`, or `TABLE` query and returns
  JSON rows plus explicit truncation metadata.

The query parser rejects multiple statements/terminators, DDL/DML (including data-modifying CTEs),
bind placeholders, row-locking clauses, and advisory-lock functions. Execution takes place in a
transaction which issues `SET TRANSACTION READ ONLY`, a short statement timeout, and a short lock
timeout. The service also caps connection count, SQL length, returned rows, per-row size, and
aggregate response bytes.

Those controls reduce mistakes and resource abuse, but PostgreSQL privileges are the hard data
boundary. Do not give the login direct table access or rely on prompt instructions to hide fields.

## Recommended PostgreSQL roles and views

Use a non-login view owner and a separate login role. The view owner gets narrowly selected source
table privileges; the login gets only access to the curated views. Adapt names and columns to the
deployed ShieldBattery schema, and keep this SQL in production database administration rather than
the ShieldBattery application migration sequence.

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

-- Grant the non-login owner only the source relations needed by the curated views.
GRANT SELECT ON public.users, public.games, public.games_users
  TO adjutant_view_owner;

CREATE VIEW adjutant_diagnostics.users
  WITH (security_barrier = true)
AS
SELECT id, name, created
FROM public.users;

CREATE VIEW adjutant_diagnostics.games
  WITH (security_barrier = true)
AS
SELECT id, start_time, map_id, config, game_length, results
FROM public.games;

CREATE VIEW adjutant_diagnostics.game_users
  WITH (security_barrier = true)
AS
SELECT game_id, user_id, start_time, selected_race, assigned_race,
       reported_results, result, apm
FROM public.games_users;

ALTER VIEW adjutant_diagnostics.users OWNER TO adjutant_view_owner;
ALTER VIEW adjutant_diagnostics.games OWNER TO adjutant_view_owner;
ALTER VIEW adjutant_diagnostics.game_users OWNER TO adjutant_view_owner;

GRANT SELECT ON ALL TABLES IN SCHEMA adjutant_diagnostics TO adjutant_diagnostics;
```

Add explicit views for desync events, matchmaking formations/ratings, ladder changes, and other
diagnostic data as needed. Name every projected column. In particular, never expose
`users_private`, password/auth/session/token material, email addresses, signup/login IPs, payment
data, or unrestricted free-form administrative tables. Review each new view exactly as you would a
new production API response.

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

Copy `.env.mcp.example` to `.env.mcp`, put the dedicated role's URL in
`ADJUTANT_MCP_DATABASE_URL`, and restrict the file to the deployment account. For Tailnet developer
access, also set `ADJUTANT_MCP_TAILSCALE_HOSTNAME` to the exact `*.ts.net` FQDN reported by
`tailscale serve status`; this is an allowlisted HTTP Host, not an additional credential. This file
is loaded only by the MCP container; neither the Discord bot nor Codex receives it.

The checked-in `config/codex.toml` makes this server required and allowlists only its two tools.
Additional production MCPs, such as Datadog, should be added to that file with equally narrow tool
and approval policies. Keep credentials in the Codex volume or environment references, never in
Git.

For developer use, any Streamable HTTP MCP client can use the Tailnet HTTPS URL without a bearer
token. Grant port 8443 only to the developer/staff identities that should be able to query the
curated views. For example, a developer can add this to their Codex `config.toml`:

```toml
[mcp_servers.shieldbattery_database]
url = "https://<adjutant-node>.<tailnet>.ts.net:8443/mcp"
enabled_tools = ["database_schema", "query_database"]
default_tools_approval_mode = "writes"
```

Tailscale Serve attaches peer identity headers to proxied requests. The MCP records them in its
structured request logs for audit context but does not treat caller-controlled headers as
authorization; the Tailnet ACL remains the access check.
