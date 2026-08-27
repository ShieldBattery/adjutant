# ShieldBattery internal bug-report API handoff

Adjutant needs a stable service-to-service replacement for the current staff browser API. The
existing Discord webhook includes `/admin/bug-reports/<uuid>`, while the private ZIP remains in the
ShieldBattery file store. A rotating staff JWT or direct object-store credentials should not be
placed in the bot.

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

Use both controls:

- Reject any request containing `X-Forwarded-For`, matching the existing `/metrics` convention so
  requests arriving through public nginx cannot reach the handler.
- Disable the routes unless `SB_INTERNAL_API_TOKEN` is configured and require
  `Authorization: Bearer <token>` using a constant-time comparison. Tailscale ACLs should restrict
  the Adjutant VM to app-server port 80 as well.

Pass `SB_INTERNAL_API_TOKEN` only to the Node app-server container. Do not log the Authorization
header. Set `Cache-Control: private, no-store` and `X-Content-Type-Options: nosniff` on successful
responses.

Recommended status codes:

- `401` for a missing or invalid bearer token on a direct connection
- `403` for a reverse-proxied request
- `404` for a malformed/unknown UUID or when the feature is disabled
- `405` for non-GET methods
- `410` when `logs_deleted` is true

## Acceptance tests

- Exact bearer tokens succeed; missing, prefixed/suffixed, and wrong tokens fail.
- Metadata and `/logs` paths accept a UUID and reject extended/malformed paths.
- Requests with `X-Forwarded-For` fail even with a valid token.
- An unknown report is `404`; deleted logs are `410`.
- The ZIP response matches the stored bytes and has no-store/nosniff headers.
- Requests never pass through to the public route stack after the internal handler responds.

Adjutant configuration maps directly to this contract:

```dotenv
SHIELDBATTERY_INTERNAL_URL=http://sb-prod
SHIELDBATTERY_INTERNAL_TOKEN=<same high-entropy value>
```

