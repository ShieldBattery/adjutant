# Datadog MCP

Adjutant uses [Datadog's managed remote MCP](https://docs.datadoghq.com/mcp_server/) for production
logs, traces, metrics, events, monitors, and service relationships. Nothing in this repository
implements or vendors Datadog's MCP server. A small Caddy sidecar holds the Datadog credential and
proxies only the Streamable HTTP MCP path:

```text
Codex -> http://127.0.0.1:8082/v1/mcp -> credential proxy -> Datadog managed MCP
```

Codex and model-generated commands never receive the credential. The proxy has no published or
Tailscale Serve port and accepts only `/v1/mcp` on shared loopback. Its fixed upstream toolset is
`core`; the checked-in Codex configuration further allowlists individual read-only tools.

## Datadog identity and permissions

Create a dedicated Datadog service account and a custom role. Give the role `MCP Read`, never
`MCP Write`, plus only the underlying read permissions needed by the checked-in tools:

- Logs Read Data and Logs Read Index Data for log search.
- APM Read for span search, traces, and service dependencies.
- Metrics and Timeseries for metric search/query, log analysis, events, and host context.
- Events and Monitors Read for production changes and alert state.
- Hosts Read for host discovery and health context.
- Service Catalog Read and Teams Read for service discovery and dependencies.

Datadog checks both the MCP permission and each product's ordinary resource permission. Review the
current requirements in the [Datadog MCP tool reference](https://docs.datadoghq.com/mcp_server/tools/)
when changing the allowlist.

Create a Service Access Token for that service account. Datadog recommends a service token for a
non-interactive service and accepts it as an `Authorization: Bearer` credential without an API key.
Do not use a staff member's OAuth login, API key, or application key for the deployed bot.

## Deployment configuration

On the VM, copy `datadog-mcp.env.example` to the ignored `datadog-mcp.env`, restrict it to the
deployment account, and fill in:

```dotenv
DATADOG_MCP_HOST=mcp.datadoghq.com
DATADOG_MCP_ACCESS_TOKEN=<service-access-token>
```

The shown host is Datadog US1. Select the Datadog site in the
[official setup guide](https://docs.datadoghq.com/mcp_server/setup/) and use the exact managed MCP
hostname for that site, without a scheme, port, path, query, or whitespace. The tracked Caddyfile
fixes the scheme to HTTPS, verifies the upstream certificate, overwrites any inbound authorization
header, and forwards only `/v1/mcp`.

The role is the hard write boundary. `deployment/config/codex.toml` is an additional reviewed
boundary: it enables only named diagnostic tools, marks the MCP required, and requires approval for
any tool advertised as a write. Adjutant has no interactive approval flow, so an accidentally
enabled write tool fails closed even before Datadog rejects it.

The VM needs outbound DNS and TCP 443 access to the selected Datadog MCP host. It needs no inbound
port for Datadog. Do not add this proxy to either Tailscale Serve configuration.

## Verification and rotation

After deployment:

```sh
docker compose ps
docker compose logs --tail=100 datadog-mcp-proxy
docker compose run --rm adjutant codex mcp get datadog
```

Then send a staff-only test request asking Adjutant to find a known, non-sensitive ShieldBattery
log event in a narrow time range. Confirm that the run inspector records the Datadog MCP call and
that Datadog's Audit Trail records an `MCP Server` event. Datadog also documents MCP usage metrics
on its [managed MCP overview](https://docs.datadoghq.com/mcp_server/).

To rotate the token, create a replacement first, update only `datadog-mcp.env`, and recreate the
proxy:

```sh
docker compose up -d --force-recreate datadog-mcp-proxy
docker compose ps datadog-mcp-proxy adjutant
```

Run the narrow test again, then revoke the old token. Never print the environment file, proxy
environment, or token in deployment logs or an Adjutant request.
