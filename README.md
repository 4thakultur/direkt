# Direkt

A self-hostable direct download manager for homelabs, written in Rust.

> **Status:** early development. HTTP/HTTPS downloads, 1fichier premium
> downloads, the web UI (with a confirmation step showing type, title, size
> and artwork), the API, and media organization into Jellyfin/Plex libraries
> work, packaged for Docker Compose. Telegram notifications are planned after
> v1.

## Quick start (Docker Compose)

```sh
cp .env.example .env               # set PUID/PGID, folders, optional keys
mkdir -p data downloads media/movies media/tv media/unsorted
docker compose up -d --build
docker compose exec direkt cat /data/setup-token
```

Then:

1. Open `http://<host>:9877`.
2. Enter the one-time setup token and create the administrator account.
3. Paste a URL and download.

[`docs/docker.md`](docs/docker.md) covers volumes, permissions, upgrades,
backups and troubleshooting.

To require a code from an authenticator app (Google Authenticator,
Microsoft Authenticator, …) at sign-in, turn on two-factor authentication
under Settings.

Forgot the password? Run `direkt reset-admin` and set up again. Lost the
authenticator app and its recovery codes? Run `direkt reset-2fa`. See
[`docs/security.md`](docs/security.md).

## Development

```sh
cargo run                         # reads ./direkt.toml if present, else defaults
                                  # (state in ./data, files in ./downloads)
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
| Deployment   | Docker Compose; Debian slim runtime image, non-root       |

## License

Direkt is free software, licensed under the
[GNU Affero General Public License v3.0](LICENSE) (`AGPL-3.0-only`). If you
run a modified version as a network service, you must offer its users the
corresponding source code.
