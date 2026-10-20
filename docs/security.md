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
- After 5 failed login (or setup-token) attempts, a client is locked out for
  15 minutes. Behind a reverse proxy every client shares the proxy's address,
  so the lockout then applies to everyone.

## Provider credentials

Provider API keys (currently 1fichier) are entered on the Settings page, or
supplied as environment variables, which take precedence. They are stored in
the database in plain text, like a configuration file would hold them, so
restrict access to `data_dir` and to its backups. Once saved, a key is never
shown again (the UI displays `************************`), returned by the API,
or written to the logs.

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
