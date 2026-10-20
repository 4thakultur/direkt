# API

The machine-readable specification is served by a running instance at
`/api/openapi.json` (OpenAPI 3.1). It requires no authentication.

## Authentication

Every endpoint under `/api/v1` needs one of these:

- **API token**: `Authorization: Bearer dkt_…`. Create tokens in the web UI
  under *Settings*, or with `POST /api/v1/tokens`. A token is shown only once,
  and only its hash is stored.
- **Session cookie**: set by logging in to the web UI. Requests that change
  something (POST/PUT/PATCH/DELETE) and are authenticated by cookie must come from the
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
| POST | `/api/v1/previews` | Look a link up before downloading: `{"url": "https://…", "password": "…"}`. Returns provider, `filename`, `size_bytes` (`size_approximate` for 1fichier), detected `media_type`, and `title`, `year` and `artwork_id` when found, plus `notes` explaining anything missing and `accepts_password` (whether a file password can be given). See [artwork.md](artwork.md). The password is optional here and stays on the server. |
| PUT | `/api/v1/previews/{id}` | Change a preview's type: `{"media_type": "game"}`. Title and artwork are looked up again. |
| POST | `/api/v1/downloads` | Submit `{"url": "https://…", "password": "…", "media_type": "movie"}`, or confirm a preview with `{"preview_id": "…", "media_type": "tv", "password": "…"}` (within an hour, once; a `password` here replaces the preview's). `password` is only for providers that support it, such as 1fichier; `media_type` is optional (`movie`, `tv`, `game`, `music`, `software`, `unsorted`). Returns `201` with the download, including `media_type`, `title` and `artwork_id`. |
| GET | `/api/v1/downloads?state=queued,downloading&limit=50&offset=0` | List downloads, newest first |
| GET | `/api/v1/downloads/{id}` | One download, with live speed while running |
| GET | `/api/v1/downloads/{id}/events` | Event history |
| POST | `/api/v1/downloads/{id}/pause` | Pause (keeps partial data) |
| POST | `/api/v1/downloads/{id}/resume` | Resume a paused download |
| POST | `/api/v1/downloads/{id}/cancel` | Cancel and delete partial data |
| POST | `/api/v1/downloads/{id}/organize` | Organize a completed download again after organizing `failed` or was `skipped` (see [media-organization.md](media-organization.md)). `409` if it is organized or already waiting. |
| POST | `/api/v1/downloads/{id}/retry` | Re-queue a failed, blocked, or cancelled download. Optional body `{"password": "…"}` replaces the download password. |
| DELETE | `/api/v1/downloads/{id}?delete_file=true` | Delete a cancelled or completed download and its history. For a completed download the file stays on disk unless `delete_file=true`; Direkt only deletes a regular file directly inside the download directory. `204`; `409` if it is in another state or being organized. If the file can't be deleted, the download is kept and the error says why. |
| DELETE | `/api/v1/downloads/cancelled` | Delete every cancelled download. Returns `{"deleted": n}`. |
| DELETE | `/api/v1/downloads/completed` | Remove every completed download from the list. Files stay on disk. Returns `{"deleted": n}`. |
| GET | `/api/v1/stats` | Transfer throughput since start: current, peak and total bytes, free space in the download directory, and a per-second speed history of up to 5 minutes (the web UI's graph). Counters reset when Direkt restarts. |
| GET | `/api/v1/tokens` | List API tokens (no secrets) |
| POST | `/api/v1/tokens` | Create `{"name": "…"}`. The secret is in this response only. |
| DELETE | `/api/v1/tokens/{id}` | Revoke a token |
| GET | `/api/v1/settings/onefichier` | Whether a 1fichier API key is configured, and its source. The key is shown only as `************************`. |
| PUT | `/api/v1/settings/onefichier` | Set the key: `{"api_key": "…"}`. Returns `409` if it comes from `DIREKT_ONEFICHIER_API_KEY`. |
| DELETE | `/api/v1/settings/onefichier` | Remove the stored key |
| POST | `/api/v1/settings/onefichier/account` | Sign in to 1fichier: `{"email": "…", "password": "…", "code": "123456"}` (`code` only for two-factor accounts). Returns `two_factor_required: true` when a code is needed; repeat the request with `code` within 10 minutes. |
| DELETE | `/api/v1/settings/onefichier/account` | Sign out and delete the stored email, password and session |
| POST | `/api/v1/settings/onefichier/cdn/refresh` | Read the CDN credit balance again (shown as `cdn` in `GET /api/v1/settings/onefichier`). Returns `429` within 5 minutes of the last check, and `409` if neither signed in nor a key is set. |
| GET | `/api/v1/settings/misc` | Miscellaneous settings: `speed_unit` (`binary`, `decimal` or `bits`: how the web UI writes speeds), `speed_limit` (bytes per second for all downloads together, `null` when unlimited), `max_concurrent`, `default_max_concurrent` (from the configuration file) and `confirm_downloads` (the web UI's confirmation menu). |
| PATCH | `/api/v1/settings/misc` | Change any of `speed_unit`, `speed_limit` (at least 10000, or 0 to remove the limit), `max_concurrent` (1–32) and `confirm_downloads`. Fields left out keep their value; nothing changes if one is invalid. Applies to running downloads immediately. |
| GET | `/api/v1/settings/tmdb` | Whether a TMDB key (for [artwork](artwork.md)) is configured, and its source. Masked like the 1fichier key. |
| PUT | `/api/v1/settings/tmdb` | Set the key: `{"api_key": "…"}` (API key or read access token). Returns `409` if it comes from `DIREKT_TMDB_API_KEY`. |
| DELETE | `/api/v1/settings/tmdb` | Remove the stored key. Cached artwork stays. |
| GET | `/api/v1/settings/library` | The [media library](media-organization.md) folders, read-only: `movies`, `tv` and `unsorted` (fallback), each with `path` (`null` when unset), the environment `variable` that sets it, and `problem` (what was wrong with it at startup, if anything). They can only be set at launch with `DIREKT_MOVIES_DIR`, `DIREKT_TV_DIR` and `DIREKT_UNSORTED_DIR`. |

Download passwords are write-only: no endpoint ever returns them.

Download states are described in [state-machine.md](state-machine.md).
Responses contain only a redacted form of the submitted URL (`display_url`).
The query string is replaced with `?…`, because it often carries access
tokens.

## Example

```sh
TOKEN=dkt_...
curl -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"url": "https://example.com/file.iso"}' \
     http://direkt.lan:9877/api/v1/downloads
```
