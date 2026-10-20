# Direkt

A self-hostable direct download manager for homelabs, written in Rust.

> **Status:** early development. HTTP/HTTPS downloads, 1fichier premium
> downloads, the web UI (with a confirmation step showing type, title, size
> and artwork), the API, and media organization into Jellyfin/Plex libraries
> work. Docker packaging is next. Telegram notifications are planned after v1.

## First run

1. Start Direkt and open `http://<host>:9877`.
2. Enter the one-time setup token from `<data_dir>/setup-token` (the log
   shows the path) and create the administrator account.
3. Paste a URL and download.

To require a code from an authenticator app (Google Authenticator,
Microsoft Authenticator, …) at sign-in, turn on two-factor authentication
under Settings.

Forgot the password? Run `direkt reset-admin` and set up again. Lost the
authenticator app and its recovery codes? Run `direkt reset-2fa`. See
[`docs/security.md`](docs/security.md).

## Development

```sh
cargo run                         # reads ./direkt.toml if present, else defaults
DIREKT_CONFIG=/path/to/direkt.toml cargo run
cargo test
cargo clippy --all-targets
```

See [`direkt.example.toml`](direkt.example.toml) for every configuration option, and
[`docs/state-machine.md`](docs/state-machine.md) for the download lifecycle, and
[`docs/download-engine.md`](docs/download-engine.md) for resume, retry, and
network-security behavior, [`docs/api.md`](docs/api.md) for the HTTP API, and
[`docs/providers/1fichier.md`](docs/providers/1fichier.md) for 1fichier, and
[`docs/artwork.md`](docs/artwork.md) for the download confirmation step
(type detection, TMDB and Steam artwork), and
[`docs/media-organization.md`](docs/media-organization.md) for moving
movies and episodes into a Jellyfin/Plex library.

`cargo test` needs no network access. One opt-in test downloads a real file
over HTTPS: `cargo test -- --ignored real_https`.

## Stack

| Concern      | Choice                                                    |
|--------------|-----------------------------------------------------------|
| Runtime      | tokio                                                     |
| HTTP server  | axum, with OpenAPI via utoipa                             |
| Web UI       | Server-rendered templates + htmx, embedded in the binary; two themes, chosen per browser under Settings → Appearance: "Steve" (after iOS and macOS, light and dark, the default) and "Beyond" (black and red); no external assets |
| Persistence  | SQLite (sqlx, WAL mode, embedded migrations)              |
| Outbound HTTP| reqwest with SSRF-checked DNS resolution and redirects    |
| Config       | One TOML file; secrets may come from `DIREKT_*` env vars  |
| Auth         | Admin login (session cookie) + revocable API tokens       |
