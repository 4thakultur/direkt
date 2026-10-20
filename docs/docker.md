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

| Container path      | `.env` setting (default)               | Holds |
|---------------------|----------------------------------------|-------|
| `/data`             | `HOST_DATA_DIR` (`./data`)             | SQLite database, setup token, artwork cache. **Back this up.** |
| `/downloads`        | `HOST_DOWNLOADS_DIR` (`./downloads`)   | Completed downloads; partial ones in `/downloads/.incomplete` |
| `/media/movies`     | `HOST_MOVIES_DIR` (`./media/movies`)   | Movie library |
| `/media/tv`         | `HOST_TV_DIR` (`./media/tv`)           | TV library |
| `/media/unsorted`   | `HOST_UNSORTED_DIR` (`./media/unsorted`) | Videos that couldn't be identified with confidence |
| `/etc/direkt/direkt.toml` | (built in)                       | Configuration; optional bind mount, see below |

Two kinds of variables are involved:

- **`HOST_*` in `.env`** are read by Docker Compose only. They choose which
  host folder is mounted at each container path (and `HOST_PORT` the host
  port). Use your real folder names, which are case-sensitive, for example
  `HOST_TV_DIR=/srv/media/TV Shows`.
- **`DIREKT_*`** are read by Direkt inside the container. The library ones
  in `compose.yaml` (`DIREKT_MOVIES_DIR=/media/movies` and so on) are
  container paths that match the mounts; leave them as they are.

Direkt only sees, checks and logs container paths, never host paths.

Every host folder must exist before the first start: the mounts set
`create_host_path: false`, so Compose refuses to start instead of creating a
missing one owned by root. Direkt likewise creates movie, show and season
folders inside the libraries but never the library folders themselves, so a
missing mount can't fill your system disk. Point Jellyfin or Plex at the
same host folders. To run without a media library, delete the three
`DIREKT_*_DIR` lines and the three library mounts in `compose.yaml`;
completed files then stay in `/downloads`.

Each mount is separate, so organizing a file copies it from `/downloads`
into the library and then deletes the original, even when both are on the
same disk. That takes a while for large files; the download stays completed
in the meantime.

### Upgrading from the older variable names

`.env` files written for earlier versions keep working: `DIREKT_PORT`,
`DIREKT_DATA_DIR` and `DIREKT_DOWNLOADS_DIR` are used when the `HOST_*`
setting is unset, and `DIREKT_MEDIA_DIR` provides the libraries as its
`movies`, `tv` and `unsorted` subfolders. If your library folders are named
differently (`Movies`, `TV Shows`), set `HOST_MOVIES_DIR`, `HOST_TV_DIR` and
`HOST_UNSORTED_DIR` to them.

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
| Settings → Media library shows a folder as failing | It doesn't exist inside the container: check `HOST_MOVIES_DIR`, `HOST_TV_DIR` and `HOST_UNSORTED_DIR` in `.env`, including upper and lower case |
| Login doesn't stick | `server.secure_cookies = true` over plain HTTP; turn it off or use HTTPS |
| Downloads from a LAN host are refused | Outbound access to private networks is off by default; see `network.allowed_private_networks` |
| Container shown as `unhealthy` | `docker compose logs direkt` shows why; often a full or unavailable `/data` |
