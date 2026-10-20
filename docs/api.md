# API

The machine-readable specification is served by a running instance at
`/api/openapi.json` (OpenAPI 3.1). It requires no authentication.

## Authentication

Every endpoint under `/api/v1` needs one of these:

- **API token**: `Authorization: Bearer dkt_…`. Create tokens in the web UI
  under *Settings*, or with `POST /api/v1/tokens`. A token is shown only once,
  and only its hash is stored.
- **Session cookie**: set by logging in to the web UI. Requests that change
  something (POST/DELETE) and are authenticated by cookie must come from the
  same origin, judged by `Sec-Fetch-Site`, or else `Origin` checked against
  `Host`. Cross-site requests are rejected with `403 forbidden`.

`GET /healthz` and `GET /api/openapi.json` are public.

## Errors

Every error uses the same body:

```json
{"error": {"kind": "invalid_request", "message": "unsupported URL scheme 'ftp'; only http and https are allowed"}}
```

| Status | `kind` | Meaning |
|--------|--------|---------|
| 400 | `invalid_request` | Malformed body, parameter, or URL (including URLs to private addresses) |
| 401 | `unauthorized` | Missing or invalid token or session |
| 403 | `forbidden` | Cookie-authenticated cross-site request |
| 404 | `not_found` | No such download, token, or endpoint |
| 409 | `conflict` | The action isn't allowed in the download's current state |
| 413/415/422 | `invalid_request` | Body too large, wrong content type, or missing fields |
| 422 | `unsupported_provider` | No provider handles the URL |
| 429 | `rate_limited` | Too many failed login attempts |
| 503 | `persistence` | Database unavailable |

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| POST | `/api/v1/downloads` | Submit `{"url": "https://…"}`. Returns `201` with the download. |
| GET | `/api/v1/downloads?state=queued,downloading&limit=50&offset=0` | List downloads, newest first |
| GET | `/api/v1/downloads/{id}` | One download, with live speed while running |
| GET | `/api/v1/downloads/{id}/events` | Event history |
| POST | `/api/v1/downloads/{id}/pause` | Pause (keeps partial data) |
| POST | `/api/v1/downloads/{id}/resume` | Resume a paused download |
| POST | `/api/v1/downloads/{id}/cancel` | Cancel and delete partial data |
| POST | `/api/v1/downloads/{id}/retry` | Re-queue a failed, blocked, or cancelled download |
| GET | `/api/v1/tokens` | List API tokens (no secrets) |
| POST | `/api/v1/tokens` | Create `{"name": "…"}`. The secret is in this response only. |
| DELETE | `/api/v1/tokens/{id}` | Revoke a token |

Download states are described in [state-machine.md](state-machine.md).
Responses contain only a redacted form of the submitted URL (`display_url`).
The query string is replaced with `?…`, because it often carries access
tokens.

## Example

```sh
TOKEN=dkt_...
curl -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"url": "https://example.com/file.iso"}' \
     http://direkt.lan:8080/api/v1/downloads
```
