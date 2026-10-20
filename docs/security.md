# Security and access

## First-run setup

When no administrator exists, Direkt writes a random one-time setup token to
`<data_dir>/setup-token` (mode `0600`) and logs that file's path, never the
token itself. Open the web UI, enter the token, and choose a username and
password. Afterwards the token file is deleted.

This keeps a stranger from claiming a freshly started instance that is already
reachable from the internet.

With Docker Compose:

```sh
docker compose exec direkt cat /data/setup-token
```

## Lost password

```sh
direkt reset-admin            # or: docker compose exec direkt direkt reset-admin
```

This deletes the administrator account, every session, and every API token.
Restart Direkt and complete setup again.

## Sessions and tokens

- Passwords are hashed with Argon2id. Sessions last 30 days. Changing the
  password ends every other session.
- The session cookie is `HttpOnly` and `SameSite=Strict`. Set
  `server.secure_cookies = true` when the UI is reached over HTTPS.
- Session and API tokens are 256-bit random values. Only their SHA-256 hashes
  are stored.
- Each client gets 5 unsuccessful login, setup-token, or password-change
  attempts per 15 minutes. Attempts are counted *before* the password is
  checked, so parallel requests cannot exceed the limit. IPv6 clients are
  counted per /64. If the limiter is full of active clients (a distributed
  attack), new clients are refused rather than tracked without bound. Behind
  a reverse proxy every client shares the proxy's address, so the limit then
  applies to everyone.
- At most two password hashes (Argon2id) run at a time, so a burst of login
  requests cannot exhaust CPU or memory.

## Provider credentials

Provider credentials (the 1fichier API key, or the 1fichier account email,
password and session) and the TMDB API key used for artwork are entered on the
Settings page. The API keys can instead come from environment variables,
which take precedence. They are
stored in the database in plain text so that Direkt can sign
in again unattended. Restrict access to `data_dir` and its backups. If the
database leaks, the 1fichier password leaks too: use a password that isn't
used anywhere else. Once saved, keys and passwords are never shown again (the
UI displays `************************`), returned by the API, or written to the
logs.

## Download previews and artwork

Clicking Download inspects the link before anything is queued: one request for
its first byte (direct links), or one anonymous visit to the 1fichier file
page. Both go through the network-policy-enforcing client, so previews cannot
reach private addresses either. The file password is kept server-side with the
preview (in memory, one hour, single use) and never sent back to the browser.

Titles parsed from file names go to TMDB (movies and series, with a TMDB key
set) and the Steam store search (games), never download URLs. Images are
fetched through the same policy-enforcing client, limited to 4 MiB, accepted
only as JPEG or PNG, and served to the browser from Direkt's own cache. At
most four previews and two metadata lookups run at once. See
[artwork.md](artwork.md).

## Files on disk

The data directory (`storage.data_dir`) holds the database and the setup
token, so Direkt makes it owner-only (`0700`) at every start, and the database
files (`direkt.db`, `-wal`, `-shm`) and setup token `0600`. Downloaded files
use the normal umask so media servers can read them.

## HTTP server limits

- A connection with no traffic for 120 seconds is closed. Once a request has
  started, its headers must arrive within 30 seconds.
- At most 512 connections at once, and 64 per client address (IPv6: per
  /64). Further connections wait (global limit) or are refused (per-client
  limit).
- Request bodies are limited to 2 MiB.

## Code

Direkt's own code contains no `unsafe` (`#![forbid(unsafe_code)]`).
Dependencies are checked with `cargo audit` against the RustSec advisory
database.

## Browser hardening

Every response carries a strict Content-Security-Policy (no inline scripts,
styles, or `eval`), `X-Frame-Options: DENY`, `nosniff`, and
`Referrer-Policy: no-referrer`. API and page responses are `no-store`.
Cookie-authenticated requests that change state must come from the same
origin.

## Remote access

Direkt doesn't terminate TLS. If you expose it beyond your LAN, put it behind
a reverse proxy with HTTPS (Caddy, Traefik, nginx), and set
`server.secure_cookies = true`. Better still, reach it over a VPN such as
WireGuard or Tailscale. Don't rely on Docker networking alone for access
control.

Outbound downloads are restricted to public addresses by default. See
[download-engine.md](download-engine.md#network-security-ssrf).
