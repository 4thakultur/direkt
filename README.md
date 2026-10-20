# Direkt

A self-hostable direct download manager for homelabs, written in Rust.

> **Status:** early development. The persistence layer, state machine,
> configuration, and HTTP/HTTPS download engine exist; the API and web UI are
> in progress.

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
network-security behavior.

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
