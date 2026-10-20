# Running Direkt with Docker Compose

Docker Compose is the supported way to run Direkt. The repository ships a
`Dockerfile`, a `compose.yaml` and an `.env.example`.

## Install

```sh
git clone <this repository> direkt && cd direkt
cp .env.example .env               # set PUID/PGID, folders, optional keys
mkdir -p data downloads media/movies media/tv media/unsorted
docker compose up -d --build
docker compose logs -f direkt      # wait for "listening"
```

Open `http://<host>:9877`, enter the setup token and create the
administrator account:

```sh
docker compose exec direkt cat /data/setup-token
```

## Volumes

| Container path      | Host (default)  | Holds |
|---------------------|-----------------|-------|
| `/data`             | `./data`        | SQLite database, setup token, artwork cache. **Back this up.** |
| `/downloads`        | `./downloads`   | Completed downloads; partial ones in `/downloads/.incomplete` |
| `/media`            | `./media`       | The media library: `movies`, `tv`, `unsorted` |
| `/etc/direkt/direkt.toml` | (built in) | Configuration; optional bind mount, see below |

Set the host side with `DIREKT_DATA_DIR`, `DIREKT_DOWNLOADS_DIR` and
`DIREKT_MEDIA_DIR` in `.env`. Paths inside the container are what Direkt
sees and logs; they are not the host paths.

The `movies`, `tv` and `unsorted` folders must exist: Direkt creates the
show, movie and season folders inside them, never the library folders
themselves, so a missing mount can't fill your system disk. Point Jellyfin
or Plex at the host's `media/movies` and `media/tv`. To run without a media
library, delete the three `DIREKT_*_DIR` lines in `compose.yaml`; completed
files then stay in `/downloads`.

`/downloads` and `/media` are separate mounts, so organizing a file copies it
and then deletes the original, which takes a while for large files. To get
instant moves, mount one host directory that holds both, and point
`storage.download_dir` and `storage.incomplete_dir` at it in your own
`direkt.toml`.

## Permissions

The container runs as `PUID:PGID` from `.env` (default `1000:1000`), never as
root. Every mounted directory must be writable by that user:

```sh
id -u; id -g                       # put these in .env as PUID and PGID
sudo chown -R "$(id -u):$(id -g)" data downloads media
```

Create the host directories before the first start. If one is missing,
Docker creates it owned by root and Direkt cannot write to it; the log then
says which directory it could not create or open.

On a NAS, use a user that can write to the media share, and make sure the
media server can read what Direkt writes there.

The container also runs with a read-only root filesystem, no Linux
capabilities and `no-new-privileges`. Direkt only writes to the mounted
directories.

## Configuration

Most settings are on the web UI's Settings page and are stored in the
database. Secrets can be set in `.env` instead (see `.env.example`); a key
set there takes precedence and cannot be changed from the web UI.

The built-in configuration file is
[`direkt.example.toml`](../direkt.example.toml) with its defaults. To change
an option that exists only in the file, such as `server.secure_cookies` behind
an HTTPS reverse proxy or `network.allowed_private_networks`, copy it to
`direkt.toml`, edit it, and uncomment the `direkt.toml` volume line in
`compose.yaml`. Keep the `storage` paths pointing inside the mounts.

## Health

`GET /healthz` answers without authentication. The image's health check runs
`direkt healthcheck`, which calls it on the configured port;
`docker compose ps` shows the result.

## Stopping and restarting

`docker compose down` and `docker compose restart` stop Direkt gracefully:
running transfers stop, keep their partial data and go back to the queue,
then resume after the restart when the server allows it. `stop_grace_period` gives this 45 seconds before Docker kills the
container. Interrupted work is recovered at startup in any case.

## Upgrading

```sh
docker compose stop
cp -a data "data.backup-$(date +%F)"     # database migrations are one-way
git pull
docker compose up -d --build
docker compose logs -f direkt
```

Database migrations run automatically at startup. Upgrades never touch
`.env`, your `direkt.toml`, or anything in the mounted directories besides
the database. To roll back, check out the previous version and restore the
`data` backup made before upgrading.

## Backup and recovery

Everything Direkt needs to restore its state is in `/data`. It is a SQLite
database in WAL mode, so copy it while Direkt is stopped:

```sh
docker compose stop
tar czf "direkt-data-$(date +%F).tar.gz" data
docker compose start
```

To restore, stop Direkt, replace `data` with the backup's contents (owned by
`PUID:PGID`), and start it again. Downloads that were in progress at backup
time are re-queued and resume or restart as the server allows.

The backup contains the password hash, API tokens, the two-factor secret and
any provider keys entered in the web UI. Store it accordingly.

Downloaded files and the media library are not part of this backup. Back
them up as you would any media.

## Remote access

Direkt does not terminate TLS. Before exposing it beyond your LAN, read
[security.md](security.md#remote-access): use a VPN, or a reverse proxy with
HTTPS and `server.secure_cookies = true`.

## Troubleshooting

| Symptom | Cause |
|---------|-------|
| `creating directory /downloads/...: Permission denied` | The mounted directory isn't writable by `PUID:PGID` |
| Settings → Media library shows a folder as failing | It doesn't exist inside the container: check `DIREKT_MEDIA_DIR` and the folder names |
| Login doesn't stick | `server.secure_cookies = true` over plain HTTP; turn it off or use HTTPS |
| Downloads from a LAN host are refused | Outbound access to private networks is off by default; see `network.allowed_private_networks` |
| Container shown as `unhealthy` | `docker compose logs direkt` shows why; often a full or unavailable `/data` |
