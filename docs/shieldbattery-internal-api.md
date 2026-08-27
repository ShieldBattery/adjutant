# ShieldBattery internal bug-report API handoff

Adjutant needs a stable service-to-service replacement for the current staff browser API. The
existing Discord webhook includes `/admin/bug-reports/<uuid>`, while the private ZIP remains in the
ShieldBattery file store. Adjutant should reach this API directly through the Tailnet, without a
staff JWT, application bearer token, or direct object-store credentials.

## Requested endpoints

Add two `GET` routes to the Node app server, mounted before canonical-host redirects and browser
session/origin middleware:

### `GET /internal/bug-reports/:reportId`

Return the existing JSON representation:

```json
{
  "report": {
    "id": "123e4567-e89b-12d3-a456-426614174000",
    "submitterId": 123,
    "details": "What the reporter entered",
    "logsDeleted": false,
    "createdAt": 1787700000000
  }
}
```

Optional fields should retain the same shape as `GetBugReportResponseJson`; the endpoint does not
need to resolve user display names or return an object-store URL.

### `GET /internal/bug-reports/:reportId/logs`

Read `bug-reports/<reportId>.zip` through the configured `FileStore` and return it as
`application/zip` with an attachment `Content-Disposition`. The current upload limit is 25 MiB, so
buffering matches the existing store interface, although a future streaming `FileStore.readStream`
would reduce peak memory.

## Access boundary

Tailscale is the authorization boundary for these routes. Enforce that boundary with both network
placement and ACLs:

- Reject any request containing `X-Forwarded-For`, matching the existing `/metrics` convention so
  requests arriving through public nginx cannot reach the handler.
- Restrict the app-server port with Tailscale ACLs so only the Adjutant VM can reach it.

The app-server listener must not be directly reachable from the public internet. Keep the public
firewall limited to nginx, and expose the direct app port only on the Docker/private/Tailscale path.
The absence of `X-Forwarded-For` is a defense-in-depth signal, not an adequate network boundary by
itself. If that listener cannot be made private, use a separate tailnet-only listener. Do not expose
these Tailnet-authorized routes through a publicly reachable listener.

Requests do not need application-layer authorization. Set `Cache-Control: private, no-store` and
`X-Content-Type-Options: nosniff` on successful responses.

Recommended status codes:

- `403` for a reverse-proxied request
- `404` for a malformed or unknown UUID
- `405` for non-GET methods
- `410` when `logs_deleted` is true

## Acceptance tests

- A request from the authorized Adjutant Tailnet peer succeeds without an application credential;
  other peers are denied by the Tailscale ACL.
- Metadata and `/logs` paths accept a UUID and reject extended/malformed paths.
- Requests with `X-Forwarded-For` fail.
- An unknown report is `404`; deleted logs are `410`.
- The ZIP response matches the stored bytes and has no-store/nosniff headers.
- Requests never pass through to the public route stack after the internal handler responds.

Adjutant configuration maps directly to this contract:

```dotenv
SHIELDBATTERY_INTERNAL_URL=http://sb-prod
```
