#!/usr/bin/env python3
"""Synthetic end-to-end smoke test for deployment/sql/moderation-views.sql.

This creates one short-lived PostgreSQL container and one local MCP process. It
uses no volumes, production credentials, or existing containers/databases.
"""

from __future__ import annotations

import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time
import urllib.parse
import uuid


ROOT = Path(__file__).resolve().parents[1]
VIEWS_SQL = ROOT / "deployment" / "sql" / "moderation-views.sql"
MCP_BINARY = ROOT / "target" / "debug" / ("adjutant-mcp.exe" if os.name == "nt" else "adjutant-mcp")
CONTAINER = f"adjutant-moderation-smoke-{uuid.uuid4().hex[:12]}"
DIAGNOSTICS_PASSWORD = "synthetic-mcp-smoke-password"
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
MCP_SESSION_ID: str | None = None

EXPECTED_VIEWS = {
    "user_bans": ["id", "user_id", "start_time", "end_time", "banned_by", "unbanned_by", "unbanned_at"],
    "user_restrictions": ["id", "user_id", "kind", "start_time", "end_time", "restricted_by", "reason"],
    "user_identifiers": ["user_id", "identifier_type", "identifier_count", "first_used", "last_used", "times_seen"],
    "user_identifier_matches": ["user_id", "linked_user_id", "identifier_type", "matching_identifier_count", "first_used", "last_used", "linked_first_used", "linked_last_used"],
    "user_identifier_bans": ["user_id", "identifier_type", "time_banned", "banned_until", "first_user_id"],
    "user_identifier_restrictions": ["id", "user_id", "identifier_type", "kind", "start_time", "end_time", "restricted_by", "first_user_id", "reason"],
    "matchmaking_bans": ["id", "user_id", "identifier_type", "triggered_by", "ban_level", "created_at", "expires_at", "clears_at", "cleared"],
    "matchmaking_completions": ["id", "user_id", "matchmaking_type", "completion_type", "search_time_millis", "completion_time", "rating"],
    "matchmaking_formations": ["game_id", "matchmaking_type", "quality", "skill_variance", "win_probability", "team_a_rating", "team_b_rating", "max_latency", "created_at", "id", "fail_phase"],
}


FIXTURE_SQL = r"""
REVOKE ALL ON DATABASE shieldbattery FROM PUBLIC;
CREATE ROLE fixture_app_owner NOLOGIN;
CREATE ROLE adjutant_view_owner
  NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS;
CREATE ROLE adjutant_diagnostics
  LOGIN PASSWORD 'synthetic-mcp-smoke-password'
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
GRANT USAGE, CREATE ON SCHEMA public TO fixture_app_owner;
GRANT USAGE ON SCHEMA public TO adjutant_view_owner;
GRANT USAGE ON SCHEMA public TO adjutant_diagnostics;
GRANT USAGE ON SCHEMA adjutant_diagnostics TO adjutant_diagnostics;
GRANT CONNECT ON DATABASE shieldbattery TO adjutant_diagnostics;

SET ROLE fixture_app_owner;
CREATE TYPE public.restriction_kind AS ENUM ('chat', 'matchmaking', 'avatar_upload', 'reporting');
CREATE TABLE public.users (
  id integer PRIMARY KEY, name text NOT NULL, email text NOT NULL,
  password_hash text NOT NULL, admin_notes text
);
CREATE TABLE public.user_bans (
  id uuid PRIMARY KEY, user_id integer NOT NULL,
  start_time timestamp without time zone NOT NULL,
  end_time timestamp without time zone NOT NULL,
  banned_by integer, reason text, unbanned_by integer,
  unbanned_at timestamp with time zone, unban_reason text
);
CREATE TABLE public.user_restrictions (
  id uuid PRIMARY KEY, user_id integer NOT NULL, kind public.restriction_kind NOT NULL,
  start_time timestamp with time zone NOT NULL, end_time timestamp with time zone NOT NULL,
  restricted_by integer, reason text, admin_notes text
);
CREATE TABLE public.user_identifiers (
  user_id integer NOT NULL, identifier_type smallint NOT NULL, identifier_hash bytea NOT NULL,
  first_used timestamp without time zone NOT NULL, last_used timestamp without time zone NOT NULL,
  times_seen integer NOT NULL,
  PRIMARY KEY (user_id, identifier_type, identifier_hash)
);
CREATE INDEX user_identifiers_identifier_type_hash_index
  ON public.user_identifiers (identifier_type, identifier_hash);
CREATE TABLE public.user_identifier_bans (
  identifier_type smallint NOT NULL, identifier_hash bytea NOT NULL,
  time_banned timestamp without time zone NOT NULL, banned_until timestamp without time zone NOT NULL,
  first_user_id integer NOT NULL, reason text,
  PRIMARY KEY (identifier_type, identifier_hash)
);
CREATE TABLE public.user_identifier_restrictions (
  id uuid PRIMARY KEY, identifier_type smallint NOT NULL, identifier_hash bytea NOT NULL,
  kind public.restriction_kind NOT NULL, start_time timestamp with time zone NOT NULL,
  end_time timestamp with time zone NOT NULL, restricted_by integer, first_user_id integer,
  reason text, admin_notes text
);
CREATE TABLE public.matchmaking_bans (
  id uuid PRIMARY KEY, identifier_type smallint NOT NULL, identifier_hash bytea NOT NULL,
  triggered_by integer, ban_level smallint NOT NULL, created_at timestamp with time zone NOT NULL,
  expires_at timestamp with time zone NOT NULL, clears_at timestamp with time zone NOT NULL,
  cleared boolean NOT NULL
);
CREATE TABLE public.matchmaking_completions (
  id uuid PRIMARY KEY, user_id integer NOT NULL, matchmaking_type text NOT NULL,
  completion_type text NOT NULL, search_time_millis integer NOT NULL,
  completion_time timestamp without time zone NOT NULL, rating real
);
CREATE TABLE public.matchmaking_match_formations (
  game_id uuid, matchmaking_type text NOT NULL, quality real NOT NULL,
  skill_variance real NOT NULL, win_probability real NOT NULL,
  team_a_rating real NOT NULL, team_b_rating real NOT NULL, max_latency real NOT NULL,
  created_at timestamp with time zone NOT NULL, id bigint PRIMARY KEY, fail_phase text
);
RESET ROLE;
REVOKE ALL ON ALL TABLES IN SCHEMA public FROM PUBLIC;

INSERT INTO public.users VALUES
  (10, 'Source', 'source@example.invalid', 'synthetic-user-password-hash', 'synthetic-admin-note'),
  (20, 'Linked', 'linked@example.invalid', 'synthetic-user-password-hash', NULL),
  (30, 'Other', 'other@example.invalid', 'synthetic-user-password-hash', NULL),
  (40, 'TypeZero', 'zero@example.invalid', 'synthetic-user-password-hash', NULL);
INSERT INTO public.user_bans VALUES
  ('00000000-0000-0000-0000-000000000001', 10, '2026-01-01 00:00:00', '2026-02-01 00:00:00', 20,
   'synthetic-ban-freeform-reason', 30, '2026-01-10 12:00:00+00', 'synthetic-unban-freeform-reason');
INSERT INTO public.user_restrictions VALUES
  ('00000000-0000-0000-0000-000000000011', 10, 'chat', '2026-01-01 00:00:00+00', '2027-01-01 00:00:00+00', 20, 'spam', 'synthetic-admin-note'),
  ('00000000-0000-0000-0000-000000000012', 20, 'chat', '2026-01-01 00:00:00+00', '2027-01-01 00:00:00+00', 10, 'cheating', 'synthetic-admin-note'),
  ('00000000-0000-0000-0000-000000000013', 30, 'matchmaking', '2026-01-01 00:00:00+00', '2027-01-01 00:00:00+00', 10, 'cheating', 'synthetic-admin-note'),
  ('00000000-0000-0000-0000-000000000014', 40, 'reporting', '2026-01-01 00:00:00+00', '2027-01-01 00:00:00+00', 10, 'spam', 'synthetic-admin-note');
INSERT INTO public.user_identifiers VALUES
  (10, 1, decode('aa', 'hex'), '2026-01-01 00:00:00', '2026-01-03 00:00:00', 2),
  (10, 1, decode('ab', 'hex'), '2026-01-02 00:00:00', '2026-01-04 00:00:00', 3),
  (20, 1, decode('aa', 'hex'), '2026-01-02 00:00:00', '2026-01-05 00:00:00', 4),
  (20, 1, decode('ab', 'hex'), '2026-01-03 00:00:00', '2026-01-06 00:00:00', 5),
  (10, 2, decode('cc', 'hex'), '2026-01-04 00:00:00', '2026-01-07 00:00:00', 6),
  (30, 2, decode('cc', 'hex'), '2026-01-05 00:00:00', '2026-01-08 00:00:00', 7),
  (10, 0, decode('00', 'hex'), '2026-01-01 00:00:00', '2026-01-09 00:00:00', 99),
  (40, 0, decode('00', 'hex'), '2026-01-01 00:00:00', '2026-01-09 00:00:00', 99);
INSERT INTO public.user_identifier_bans VALUES
  (1, decode('aa', 'hex'), '2026-01-01 00:00:00', '2027-01-01 00:00:00', 10, 'synthetic-ban-freeform-reason'),
  (2, decode('dd', 'hex'), '2026-01-02 00:00:00', '2026-01-03 00:00:00', 30, 'synthetic-ban-freeform-reason'),
  (0, decode('00', 'hex'), '2026-01-02 00:00:00', '2026-01-03 00:00:00', 40, 'synthetic-ban-freeform-reason');
INSERT INTO public.user_identifier_restrictions VALUES
  ('00000000-0000-0000-0000-000000000021', 1, decode('aa', 'hex'), 'chat', '2026-01-01 00:00:00+00', '2027-01-01 00:00:00+00', 20, 10, 'spam', 'synthetic-admin-note'),
  ('00000000-0000-0000-0000-000000000022', 1, decode('ab', 'hex'), 'chat', '2026-01-01 00:00:00+00', '2027-01-01 00:00:00+00', 20, 10, 'cheating', 'synthetic-admin-note'),
  ('00000000-0000-0000-0000-000000000023', 2, decode('cc', 'hex'), 'matchmaking', '2026-01-01 00:00:00+00', '2027-01-01 00:00:00+00', 20, 10, 'cheating', 'synthetic-admin-note'),
  ('00000000-0000-0000-0000-000000000024', 2, decode('ee', 'hex'), 'reporting', '2026-01-01 00:00:00+00', '2027-01-01 00:00:00+00', 20, 10, 'custom_reason', 'synthetic-admin-note'),
  ('00000000-0000-0000-0000-000000000025', 0, decode('00', 'hex'), 'avatar_upload', '2026-01-01 00:00:00+00', '2027-01-01 00:00:00+00', 20, 10, 'inappropriate_content', 'synthetic-admin-note');
INSERT INTO public.matchmaking_bans VALUES
  ('00000000-0000-0000-0000-000000000031', 1, decode('aa', 'hex'), 20, 2, '2026-01-01 00:00:00+00', '2027-01-01 00:00:00+00', '2027-02-01 00:00:00+00', false),
  ('00000000-0000-0000-0000-000000000032', 2, decode('dd', 'hex'), 20, 3, '2025-01-01 00:00:00+00', '2025-02-01 00:00:00+00', '2025-03-01 00:00:00+00', true),
  ('00000000-0000-0000-0000-000000000033', 0, decode('00', 'hex'), 20, 1, '2026-02-01 00:00:00+00', '2026-03-01 00:00:00+00', '2026-04-01 00:00:00+00', false);
INSERT INTO public.matchmaking_completions VALUES
  ('00000000-0000-0000-0000-000000000041', 10, '1v1', 'found', 123, '2026-01-01 01:00:00', 1500.5),
  ('00000000-0000-0000-0000-000000000042', 10, '1v1', 'cancel', 234, '2026-01-01 02:00:00', NULL),
  ('00000000-0000-0000-0000-000000000043', 20, '2v2', 'disconnect', 345, '2026-01-01 03:00:00', NULL);
INSERT INTO public.matchmaking_match_formations VALUES
  ('00000000-0000-0000-0000-000000000051', '1v1', 0.9, 1.1, 0.5, 1500, 1500, 42, '2026-01-01 00:00:00+00', 101, NULL),
  (NULL, '1v1', 0.8, 1.2, 0.4, 1400, 1600, 55, '2026-01-01 00:01:00+00', 102, 'accepting'),
  (NULL, '2v2', 0.7, 1.3, 0.6, 1300, 1700, 65, '2026-01-01 00:02:00+00', 103, 'drafting'),
  (NULL, '2v2', 0.6, 1.4, 0.7, 1200, 1800, 75, '2026-01-01 00:03:00+00', 104, 'loading');
"""


def fail(message: str) -> None:
    raise AssertionError(message)


def run(command: list[str], *, input_text: str | None = None, env: dict[str, str] | None = None) -> str:
    completed = subprocess.run(
        command,
        input=input_text,
        text=True,
        capture_output=True,
        check=False,
        env=env,
        creationflags=CREATE_NO_WINDOW,
    )
    if completed.returncode:
        raise RuntimeError(f"command failed ({completed.returncode}): {' '.join(command[:4])}\n{completed.stderr[-2000:]}")
    return completed.stdout


def psql(sql: str, *, login: bool = False, tcp: bool = False) -> str:
    command = ["docker", "exec", "-i"]
    env = None
    if login:
        command.extend(["--env", f"PGPASSWORD={DIAGNOSTICS_PASSWORD}"])
        env = os.environ.copy()
    command.extend([CONTAINER, "psql", "--set", "ON_ERROR_STOP=on", "--tuples-only", "--no-align"])
    if tcp:
        command.extend(["--host", "127.0.0.1"])
    command.extend(["--username", "adjutant_diagnostics" if login else "postgres", "--dbname", "shieldbattery"])
    return run(command, input_text=sql, env=env)


def psql_json(sql: str, *, login: bool = False) -> object:
    output = psql(f"COPY ({sql}) TO STDOUT;", login=login).strip()
    return json.loads(output)


def start_postgres() -> int:
    run([
        "docker", "run", "--detach", "--rm", "--name", CONTAINER,
        "--publish", "127.0.0.1::5432",
        "--tmpfs", "/var/lib/postgresql/data",
        "--env", "POSTGRES_PASSWORD=synthetic-postgres-smoke-password",
        "--env", "POSTGRES_DB=shieldbattery",
        "postgres:17-alpine",
    ])
    for _ in range(60):
        try:
            psql("SELECT 1;", tcp=True)
            port_line = run(["docker", "port", CONTAINER, "5432/tcp"]).strip().splitlines()[0]
            return int(port_line.rsplit(":", 1)[1])
        except (RuntimeError, IndexError, ValueError):
            time.sleep(0.5)
    fail("temporary PostgreSQL container did not become ready")


def free_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def rpc(port: int, payload: dict[str, object]) -> tuple[int, object | None]:
    global MCP_SESSION_ID
    body = json.dumps(payload).encode("utf-8")
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    connection.request(
        "POST", "/mcp", body=body,
        headers={
            "Accept": "application/json, text/event-stream",
            "Content-Type": "application/json",
            "Host": f"127.0.0.1:{port}",
            "MCP-Protocol-Version": "2025-03-26",
            **({"MCP-Session-Id": MCP_SESSION_ID} if MCP_SESSION_ID else {}),
        },
    )
    response = connection.getresponse()
    raw = response.read()
    status = response.status
    session_id = response.getheader("MCP-Session-Id")
    if session_id:
        MCP_SESSION_ID = session_id
    connection.close()
    if not raw:
        return status, None
    decoded = raw.decode("utf-8")
    candidates = [line[6:] for line in decoded.splitlines() if line.startswith("data: ") and line[6:].strip()]
    try:
        return status, json.loads(candidates[-1] if candidates else decoded)
    except json.JSONDecodeError as error:
        fail(f"MCP returned non-JSON HTTP body: {decoded[:500]!r} ({error})")


def tool_result(port: int, request_id: int, name: str, arguments: dict[str, object]) -> object:
    status, response = rpc(port, {
        "jsonrpc": "2.0", "id": request_id, "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    })
    if status != 200 or not isinstance(response, dict):
        fail(f"{name} did not return a JSON-RPC response (HTTP {status})")
    result = response.get("result")
    if not isinstance(result, dict):
        fail(f"{name} returned no MCP result: {response}")
    if result.get("isError"):
        return result
    content = result.get("content")
    if not isinstance(content, list) or not content or not isinstance(content[0], dict):
        fail(f"{name} returned an unexpected MCP content envelope")
    text = content[0].get("text")
    if not isinstance(text, str):
        fail(f"{name} returned non-text MCP content")
    return json.loads(text)


def query(port: int, request_id: int, sql: str, max_rows: int | None = None) -> object:
    arguments: dict[str, object] = {"sql": sql}
    if max_rows is not None:
        arguments["max_rows"] = max_rows
    return tool_result(port, request_id, "query_database", arguments)


def assert_equal(actual: object, expected: object, label: str) -> None:
    if actual != expected:
        fail(f"{label}: expected {expected!r}, got {actual!r}")


def assert_database_properties() -> None:
    expected_columns = [(name, column) for name in sorted(EXPECTED_VIEWS) for column in EXPECTED_VIEWS[name]]
    columns = psql_json("""
      SELECT coalesce(json_agg(json_build_array(table_name, column_name) ORDER BY table_name, ordinal_position), '[]'::json)
      FROM information_schema.columns
      WHERE table_schema = 'adjutant_diagnostics'
    """)
    assert_equal(columns, [list(item) for item in expected_columns], "view column projection")

    properties = psql_json("""
      SELECT coalesce(json_agg(json_build_array(c.relname, r.rolname,
        coalesce(array_to_string(c.reloptions, ','), '')) ORDER BY c.relname), '[]'::json)
      FROM pg_class AS c JOIN pg_namespace AS n ON n.oid = c.relnamespace
      JOIN pg_roles AS r ON r.oid = c.relowner
      WHERE n.nspname = 'adjutant_diagnostics' AND c.relkind = 'v'
    """)
    assert_equal([row[:2] for row in properties], [[name, "adjutant_view_owner"] for name in sorted(EXPECTED_VIEWS)], "view ownership")
    if any("security_barrier=true" not in row[2] for row in properties):
        fail(f"all moderation views must have security_barrier=true: {properties!r}")

    source_tables = sorted(["users", "user_bans", "user_restrictions", "user_identifiers", "user_identifier_bans", "user_identifier_restrictions", "matchmaking_bans", "matchmaking_completions", "matchmaking_match_formations"])
    privileges = psql_json("""
      SELECT coalesce(json_agg(json_build_array(relname, has_table_privilege('adjutant_diagnostics', pg_class.oid, 'SELECT')) ORDER BY relname), '[]'::json)
      FROM pg_class JOIN pg_namespace ON pg_namespace.oid = relnamespace
      WHERE nspname = 'public' AND relname IN (
        'users', 'user_bans', 'user_restrictions', 'user_identifiers',
        'user_identifier_bans', 'user_identifier_restrictions', 'matchmaking_bans',
        'matchmaking_completions', 'matchmaking_match_formations')
    """)
    assert_equal(privileges, [[name, False] for name in source_tables], "no base-table SELECT grant")

    hidden_columns = psql_json("""
      SELECT coalesce(json_agg(has_column_privilege('adjutant_view_owner', relation, column_name, 'SELECT')), '[]'::json)
      FROM (VALUES
        ('public.user_bans'::regclass, 'reason'), ('public.user_bans'::regclass, 'unban_reason'),
        ('public.user_restrictions'::regclass, 'admin_notes'),
        ('public.user_identifier_restrictions'::regclass, 'admin_notes')
      ) AS hidden(relation, column_name)
    """)
    assert_equal(hidden_columns, [False] * 4, "view-owner sensitive source-column grants")


def assert_mcp_session(port: int) -> None:
    initialize = {
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "moderation-smoke", "version": "1"}},
    }
    status, response = rpc(port, initialize)
    if status != 200 or not isinstance(response, dict) or "result" not in response:
        fail(f"initialize failed: HTTP {status}, {response!r}")
    status, _ = rpc(port, {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}})
    if status not in (200, 202):
        fail(f"initialized notification failed: HTTP {status}")
    status, response = rpc(port, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
    if status != 200 or not isinstance(response, dict):
        fail(f"tools/list failed: HTTP {status}")
    tools = response.get("result", {}).get("tools", [])
    names = {tool.get("name") for tool in tools if isinstance(tool, dict)}
    if not {"database_schema", "query_database"}.issubset(names):
        fail(f"tools/list omitted database tools: {names!r}")

    schema = tool_result(port, 3, "database_schema", {"schema": "adjutant_diagnostics"})
    if not isinstance(schema, dict):
        fail("database_schema returned a non-object")
    schema_names = {column["table_name"] for column in schema.get("columns", [])}
    assert_equal(schema_names, set(EXPECTED_VIEWS), "MCP-visible moderation views")

    request_id = 10
    for view in EXPECTED_VIEWS:
        result = query(port, request_id, f"SELECT * FROM adjutant_diagnostics.{view} LIMIT 1")
        request_id += 1
        if not isinstance(result, dict) or result.get("row_count", 0) != 1:
            fail(f"MCP could not read {view}: {result!r}")

    identifiers = query(port, request_id, """
      SELECT user_id, identifier_type, identifier_count, times_seen
      FROM adjutant_diagnostics.user_identifiers WHERE user_id = 10 ORDER BY identifier_type
    """)
    request_id += 1
    assert_equal(identifiers.get("rows") if isinstance(identifiers, dict) else None,
                 [{"user_id": 10, "identifier_type": 1, "identifier_count": 2, "times_seen": 5},
                  {"user_id": 10, "identifier_type": 2, "identifier_count": 1, "times_seen": 6}],
                 "identifier aggregation excludes type zero")

    matches = query(port, request_id, """
      SELECT linked_user_id, identifier_type, matching_identifier_count
      FROM adjutant_diagnostics.user_identifier_matches WHERE user_id = 10 ORDER BY linked_user_id, identifier_type
    """)
    request_id += 1
    assert_equal(matches.get("rows") if isinstance(matches, dict) else None,
                 [{"linked_user_id": 20, "identifier_type": 1, "matching_identifier_count": 2},
                  {"linked_user_id": 30, "identifier_type": 2, "matching_identifier_count": 1}],
                 "matching identifiers count types without self/type-zero links")
    type_count = query(port, request_id, """
      SELECT count(*) AS matching_type_rows, count(DISTINCT identifier_type) AS matching_types
      FROM adjutant_diagnostics.user_identifier_matches WHERE user_id = 10
    """)
    request_id += 1
    assert_equal(type_count.get("rows") if isinstance(type_count, dict) else None,
                 [{"matching_type_rows": 2, "matching_types": 2}],
                 "matching links are one row per linked account and identifier type")

    matching_types = query(port, request_id, """
      SELECT linked_user_id, count(DISTINCT identifier_type) AS matching_types
      FROM adjutant_diagnostics.user_identifier_matches
      WHERE user_id = 10 GROUP BY linked_user_id ORDER BY linked_user_id
    """)
    request_id += 1
    assert_equal(matching_types.get("rows") if isinstance(matching_types, dict) else None,
                 [{"linked_user_id": 20, "matching_types": 1},
                  {"linked_user_id": 30, "matching_types": 1}],
                 "multiple shared hashes of one type contribute only one matching type")

    punishments = query(port, request_id, """
      SELECT user_id, identifier_type, banned_until
      FROM adjutant_diagnostics.user_identifier_bans ORDER BY identifier_type, user_id NULLS LAST
    """)
    request_id += 1
    if not isinstance(punishments, dict) or [row["user_id"] for row in punishments["rows"]] != [10, 20, None]:
        fail(f"identifier-ban associations did not retain source and unmatched rows: {punishments!r}")

    matchmaking = query(port, request_id, """
      SELECT id, user_id, identifier_type, expires_at, clears_at, cleared
      FROM adjutant_diagnostics.matchmaking_bans ORDER BY id, user_id NULLS LAST
    """)
    request_id += 1
    if not isinstance(matchmaking, dict):
        fail("matchmaking_bans returned a non-object")
    rows = matchmaking["rows"]
    if [row["user_id"] for row in rows] != [10, 20, None, None] or [row["identifier_type"] for row in rows] != [1, 1, 2, 0]:
        fail(f"matchmaking direct/unmatched/type-zero records changed: {rows!r}")
    if not (rows[0]["cleared"] is False and rows[2]["cleared"] is True and rows[0]["expires_at"] and rows[2]["clears_at"]):
        fail(f"matchmaking active/expired/cleared timestamps were lost: {rows!r}")

    completions = query(port, request_id, """
      SELECT completion_type, rating FROM adjutant_diagnostics.matchmaking_completions ORDER BY id
    """)
    request_id += 1
    assert_equal(completions.get("rows") if isinstance(completions, dict) else None,
                 [{"completion_type": "found", "rating": 1500.5},
                  {"completion_type": "cancel", "rating": None},
                  {"completion_type": "disconnect", "rating": None}],
                 "matchmaking completion outcomes and historical NULL ratings")
    formations = query(port, request_id, """
      SELECT game_id, id, fail_phase FROM adjutant_diagnostics.matchmaking_formations ORDER BY id
    """)
    request_id += 1
    assert_equal(formations.get("rows") if isinstance(formations, dict) else None,
                 [{"game_id": "00000000-0000-0000-0000-000000000051", "id": 101, "fail_phase": None},
                  {"game_id": None, "id": 102, "fail_phase": "accepting"},
                  {"game_id": None, "id": 103, "fail_phase": "drafting"},
                  {"game_id": None, "id": 104, "fail_phase": "loading"}],
                 "launched and failed matchmaking formations")

    reasons = query(port, request_id, """
      SELECT user_id, kind, reason FROM adjutant_diagnostics.user_restrictions ORDER BY user_id
    """)
    request_id += 1
    assert_equal(reasons.get("rows") if isinstance(reasons, dict) else None,
                 [{"user_id": 10, "kind": "chat", "reason": "spam"},
                  {"user_id": 20, "kind": "chat", "reason": None},
                  {"user_id": 30, "kind": "matchmaking", "reason": "cheating"},
                  {"user_id": 40, "kind": "reporting", "reason": None}],
                 "per-kind user restriction reason scrubbing")

    identifier_reasons = query(port, request_id, """
      SELECT DISTINCT id, kind, reason
      FROM adjutant_diagnostics.user_identifier_restrictions ORDER BY id
    """)
    request_id += 1
    if not isinstance(identifier_reasons, dict) or [row["reason"] for row in identifier_reasons["rows"]] != ["spam", None, "cheating", None]:
        fail(f"per-kind/custom identifier restriction reasons were not scrubbed: {identifier_reasons!r}")

    inaccessible = tool_result(port, request_id, "query_database", {"sql": "SELECT * FROM public.user_identifiers"})
    request_id += 1
    if not isinstance(inaccessible, dict) or not inaccessible.get("isError"):
        fail(f"MCP unexpectedly read a base table: {inaccessible!r}")
    rejected_write = tool_result(port, request_id, "query_database", {"sql": "INSERT INTO public.users VALUES (99, 'Write', 'w@example.invalid', 'x', NULL)"})
    request_id += 1
    if not isinstance(rejected_write, dict) or not rejected_write.get("isError"):
        fail(f"MCP unexpectedly accepted a write: {rejected_write!r}")
    capped = query(port, request_id, "SELECT user_id FROM adjutant_diagnostics.user_identifier_matches", max_rows=1)
    if not isinstance(capped, dict) or capped.get("row_count") != 1 or capped.get("truncated") is not True:
        fail(f"MCP row cap/truncation failed: {capped!r}")

    leaked = json.dumps([identifiers, matches, punishments, matchmaking, reasons, identifier_reasons])
    for secret in ("synthetic-user-password-hash", "synthetic-admin-note", "synthetic-ban-freeform-reason"):
        if secret in leaked:
            fail(f"MCP moderation output leaked protected fixture value {secret!r}")
    user_count = psql("SELECT count(*) FROM public.users;", login=False).strip()
    assert_equal(user_count, "4", "MCP write rejection preserved source data")


def start_mcp(database_port: int) -> tuple[subprocess.Popen[str], int]:
    if not MCP_BINARY.is_file():
        fail(f"build the local MCP executable first: {MCP_BINARY}")
    port = free_loopback_port()
    url_password = urllib.parse.quote(DIAGNOSTICS_PASSWORD, safe="")
    environment = os.environ.copy()
    environment.update({
        "ADJUTANT_MCP_DATABASE_URL": f"postgresql://adjutant_diagnostics:{url_password}@127.0.0.1:{database_port}/shieldbattery",
        "ADJUTANT_MCP_BIND": f"127.0.0.1:{port}",
        "ADJUTANT_MCP_MAX_ROWS": "100",
        "RUST_LOG": "adjutant_mcp=warn",
    })
    process = subprocess.Popen(
        [str(MCP_BINARY)], cwd=ROOT, env=environment,
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
        creationflags=CREATE_NO_WINDOW,
    )
    try:
        for _ in range(50):
            if process.poll() is not None:
                error = process.stderr.read() if process.stderr else ""
                fail(f"MCP process exited before readiness: {error[-2000:]}")
            try:
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
                connection.request("GET", "/healthz", headers={"Host": f"127.0.0.1:{port}"})
                response = connection.getresponse()
                response.read()
                connection.close()
                if response.status == 200:
                    return process, port
            except OSError:
                pass
            time.sleep(0.2)
        fail("local MCP executable did not become ready")
    except BaseException:
        stop_process(process)
        raise


def stop_process(process: subprocess.Popen[str] | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def main() -> int:
    process: subprocess.Popen[str] | None = None
    started_container = False
    try:
        if not VIEWS_SQL.is_file():
            fail(f"missing checked-in moderation view SQL: {VIEWS_SQL}")
        started_container = True
        database_port = start_postgres()
        psql(FIXTURE_SQL)
        sql = VIEWS_SQL.read_text(encoding="utf-8")
        psql(sql)
        psql(sql)  # CREATE OR REPLACE and grants are explicitly operationally idempotent.
        assert_database_properties()
        process, mcp_port = start_mcp(database_port)
        assert_mcp_session(mcp_port)
        print("moderation MCP smoke test passed (synthetic temporary PostgreSQL + local MCP)")
        return 0
    finally:
        stop_process(process)
        if started_container:
            subprocess.run(["docker", "stop", "--time", "1", CONTAINER], capture_output=True, text=True, creationflags=CREATE_NO_WINDOW)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (AssertionError, RuntimeError, OSError, json.JSONDecodeError) as error:
        print(f"moderation MCP smoke test failed: {error}", file=sys.stderr)
        raise SystemExit(1)

