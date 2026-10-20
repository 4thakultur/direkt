# Direkt

A self-hostable direct download manager for homelabs, written in Rust.

> **Status:** early development. HTTP/HTTPS downloads, 1fichier premium
> downloads, the web UI, and the API work. Telegram notifications, media organization, and Docker packaging
> are next.

## First run

1. Start Direkt and open `http://<host>:9877`.
2. Enter the one-time setup token from `<data_dir>/setup-token` (the log
   shows the path) and create the administrator account.
3. Paste a URL and download.

Forgot the password? Run `direkt reset-admin` and set up again. See
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
[`docs/providers/1fichier.md`](docs/providers/1fichier.md) for 1fichier.

`cargo test` needs no network access. One opt-in test downloads a real file
over HTTPS: `cargo test -- --ignored real_https`.

## Stack

| Concern      | Choice                                                    |
|--------------|-----------------------------------------------------------|
| Runtime      | tokio                                                     |
| HTTP server  | axum, with OpenAPI via utoipa                             |
| Web UI       | Server-rendered templates + htmx, embedded in the binary  |
| Persistence  | SQLite (sqlx, WAL mode, embedded migrations)              |
| Outbound HTTP| reqwest with SSRF-checked DNS resolution and redirects    |
| Config       | One TOML file; secrets may come from `DIREKT_*` env vars  |
| Auth         | Admin login (session cookie) + revocable API tokens       |
