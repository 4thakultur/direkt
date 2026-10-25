# API

The machine-readable specification is served by a running instance at
`/api/openapi.json` (OpenAPI 3.1). It requires no authentication.

A readable reference, generated from the same document, is in the web UI at
`/settings/api` (linked from *Settings → API tokens*). It lists every endpoint
with its parameters, request fields, responses and a `curl` example, plus the
download states, schemas and error kinds. It requires signing in.

## Authentication

Every endpoint under `/api/v1` needs one of these:

- **API token**: `Authorization: Bearer dkt_…`. Create tokens in the web UI
  under *Settings*, or with `POST /api/v1/tokens` (needs the `api_tokens`
  permission). A token is shown only once, and only its hash is stored. It
  acts as the account that created it, with that account's permissions.
- **Session cookie**: set by logging in to the web UI. Requests that change
  something (POST/PUT/PATCH/DELETE) and are authenticated by cookie must come from the
  same origin, judged by `Sec-Fetch-Site`, or else `Origin` checked against
  `Host`. Cross-site requests are rejected with `403 forbidden`.

`GET /healthz` and `GET /api/openapi.json` are public.

Requests are checked against the account's role and permissions (see
[security.md](security.md#accounts-roles-and-permissions)): download lists
contain only the account's own downloads without `view_all_downloads`,
someone else's download answers `404` unless the account can see it and
`403` if it can see but not manage it, settings endpoints need
`manage_settings`, and `/api/v1/users` is for administrators. An account
that still has to replace its temporary password, or set up required
two-factor authentication, gets `403` everywhere until it has done so in the
web UI.

## Errors

Every error uses the same body:

```json
{"error": {"kind": "invalid_request", "message": "unsupported URL scheme 'ftp'; only http and https are allowed"}}
```

| Status | `kind` | Meaning |
|--------|--------|---------|
| 400 | `invalid_request` | Malformed body, parameter, or URL (including URLs to private addresses) |
| 401 | `unauthorized` | Missing or invalid token or session |
| 403 | `forbidden` | The account lacks the permission, or hasn't finished its first sign-in, or a cookie-authenticated cross-site request |
| 404 | `not_found` | No such download (or not one the account can see), token, account, or endpoint |
| 409 | `conflict` | The action isn't allowed in the download's current state; a username is taken; an account change would lock out the last administrator |
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
| GET | `/api/v1/downloads?state=queued,downloading&limit=50&offset=0` | List downloads, newest first: the account's own, or everyone's with `view_all_downloads`. Each has `owner_id` and `owner` (username; `null` once that account is deleted). |
| GET | `/api/v1/downloads/{id}` | One download, with live speed while running |
| GET | `/api/v1/downloads/{id}/events` | Event history |
| POST | `/api/v1/downloads/{id}/pause` | Pause (keeps partial data) |
| POST | `/api/v1/downloads/{id}/resume` | Resume a paused download |
| POST | `/api/v1/downloads/{id}/cancel` | Cancel and delete partial data |
| POST | `/api/v1/downloads/{id}/organize` | Organize a completed download again after organizing `failed` or was `skipped` (see [media-organization.md](media-organization.md)). `409` if it is organized or already waiting. |
| POST | `/api/v1/downloads/{id}/retry` | Re-queue a failed, blocked, or cancelled download. Optional body `{"password": "…"}` replaces the download password. |
| DELETE | `/api/v1/downloads/{id}?delete_file=true` | Delete a cancelled or completed download and its history. For a completed download the file stays on disk unless `delete_file=true` (needs `delete_files`); Direkt only deletes a regular file directly inside the download directory. `204`; `409` if it is in another state or being organized. If the file can't be deleted, the download is kept and the error says why. |
| DELETE | `/api/v1/downloads/cancelled` | Delete every cancelled download the account manages. Returns `{"deleted": n}`. |
| DELETE | `/api/v1/downloads/completed` | Remove every completed download the account manages from the list. Files stay on disk. Returns `{"deleted": n}`. |
| GET | `/api/v1/stats` | Transfer throughput since start: current, peak and total bytes, free space in the download directory, and a per-second speed history of up to 5 minutes (the web UI's graph). Counters reset when Direkt restarts. |
| GET | `/api/v1/tokens` | List the account's API tokens (no secrets) |
| POST | `/api/v1/tokens` | Create `{"name": "…"}`. The secret is in this response only. |
| DELETE | `/api/v1/tokens/{id}` | Revoke one of the account's tokens |
| GET | `/api/v1/me` | The signed-in account: `id`, `username`, `role`, `permissions` (all of them for administrators), and its two-factor and password state |
| GET | `/api/v1/users` | Every account (administrators only, like the rest of `/api/v1/users`) |
| POST | `/api/v1/users` | Create `{"username": "alice", "role": "user", "permissions": ["submit_downloads"], "require_two_factor": true}` (`role` defaults to `user`, `permissions` to `submit_downloads` and `premium_providers`). Returns `201` with `user`, `temporary_password` (in this response only) and `expires_at` (72 hours). |
| GET | `/api/v1/users/{id}` | One account |
| PATCH | `/api/v1/users/{id}` | Change any of `role`, `permissions` (replaces them), `require_two_factor`, `disabled`. `409` for demoting or disabling yourself, or leaving no enabled administrator. |
| DELETE | `/api/v1/users/{id}` | Delete an account with its sessions and tokens; its downloads stay without an owner. `409` for yourself. |
| POST | `/api/v1/users/{id}/reset-password` | Issue a new temporary password (like creating) and sign the account out. `409` for yourself. |
| POST | `/api/v1/users/{id}/reset-two-factor` | Turn off the account's two-factor authentication (lost phone). `409` if already off, or for yourself. |
| GET | `/api/v1/settings/onefichier` | (Every `/api/v1/settings` endpoint needs `manage_settings`.) Whether a 1fichier API key is configured, and its source. The key is shown only as `************************`. |
| PUT | `/api/v1/settings/onefichier` | Set the key: `{"api_key": "…"}`. Returns `409` if it comes from `DIREKT_ONEFICHIER_API_KEY`. |
| DELETE | `/api/v1/settings/onefichier` | Remove the stored key |
| POST | `/api/v1/settings/onefichier/account` | Sign in to 1fichier: `{"email": "…", "password": "…", "code": "123456"}` (`code` only for two-factor accounts). Returns `two_factor_required: true` when a code is needed; repeat the request with `code` within 10 minutes. |
| DELETE | `/api/v1/settings/onefichier/account` | Sign out and delete the stored email, password and session |
| POST | `/api/v1/settings/onefichier/cdn/refresh` | Read the CDN credit balance again (shown as `cdn` in `GET /api/v1/settings/onefichier`). Returns `429` within 5 minutes of the last check, and `409` if neither signed in nor a key is set. |
| GET | `/api/v1/settings/misc` | Miscellaneous settings: `speed_unit` (`binary`, `decimal` or `bits`: how the web UI writes speeds), `speed_limit` (bytes per second for all downloads together, `null` when unlimited), `max_concurrent`, `default_max_concurrent` (from the configuration file) `confirm_downloads` (the web UI's confirmation menu) and `delete_extracted_archives` (delete an archive once all its videos are in the library). |
| PATCH | `/api/v1/settings/misc` | Change any of `speed_unit`, `speed_limit` (at least 10000, or 0 to remove the limit), `max_concurrent` (1–32), `confirm_downloads` and `delete_extracted_archives`. Fields left out keep their value; nothing changes if one is invalid. Applies to running downloads immediately. |
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
