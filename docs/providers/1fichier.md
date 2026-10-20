# 1fichier

Direkt downloads 1fichier files with a **premium account** through the
official API. This is the same method JDownloader uses in its "premium API"
mode.

## Setup

1. Log in on 1fichier.com and copy your API key from
   *My account → Settings → API key*. 1fichier gives API keys only to
   customers (Premium, Premium Light, Premium GOLD, Access, CDN).
2. In Direkt, open **Settings → 1fichier**, paste the key, and click
   **Save**. It takes effect on the next download attempt, with no restart
   needed. Once saved, the field shows only `************************`. To
   replace the key, clear the field and paste the new one. **Remove key**
   deletes it.

   Alternatively, set the `DIREKT_ONEFICHIER_API_KEY` environment variable,
   for example from a Docker secret or an `.env` file. If it's set, it takes
   precedence, and the Settings field is shown locked.

The key is never logged, rendered in a page, or returned by the API, which
only reports whether a key is configured and where it comes from. A key saved
through Settings is stored in the database (`<data_dir>/direkt.db`), so
protect backups of that directory. Without a key, 1fichier links are still
accepted, but they stop in *needs attention* until a key is configured and you
retry them.

## Supported links

`https://<domain>/?<file id>`, where the file ID is 5–20 lowercase letters
and digits, on any official 1fichier domain: `1fichier.com`,
`alterupload.com`, `cjoint.net`, `desfichiers.com`, `dfichiers.com`,
`megadl.fr`, `mesfichiers.org`, `piecejointe.net`, `pjointe.com`,
`tenvoi.com`, `dl4free.com`. `www.` and `http://` are accepted, and extra
query parameters such as `&af=…` are dropped. Other links on these domains
are rejected when you submit them. Direct CDN links (such as
`https://a-12.1fichier.com/…`) are downloaded as plain HTTP.

## Password-protected files

Submit the password with the link (the UI's optional password field, or
`"password"` in `POST /api/v1/downloads`). If a download stops with
*password protected* or *password rejected*, retry it with the password:

```sh
curl -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
     -d '{"password": "the-password"}' \
     http://direkt.lan:9877/api/v1/downloads/<id>/retry
```

1fichier answers "password required" for every kind of access-restricted
file, not only password-protected ones.

## How it works

Each attempt calls `POST https://api.1fichier.com/v1/download/get_token.cgi`
once, with `{"url": "<link>", "pass": "<password>"}`. The response is a direct
link that is valid for 5 minutes, and the normal download engine then
fetches it. 1fichier supports byte ranges and allows resuming for 30 minutes.
A resume or retry therefore asks for a fresh link and continues the partial
file with a `Range` request. Each attempt gets a different link, so instead of
an ETag the server-reported total size must match the partial download exactly,
otherwise the download starts over. If a link returns a web page instead of
the file, it is never saved.

Direkt does not call `file/info.cgi` (JDownloader found that it triggers IP
blocks) or `user/info.cgi` (limited to once every 5 minutes per account). As a
result, the file name and size become known once the transfer starts.

## Rate limits and errors

1fichier allows at most 3 API requests per second, and it temporarily bans the
IP address and account after repeated error responses. Direkt makes at most
one API request per second and never retries an error automatically if
retrying wouldn't help:

| 1fichier says | Direkt does |
|---------------|-------------|
| `Flood detected …`, HTTP 429 | Waits 5 minutes, then retries |
| `IP Locked` | Waits 60 minutes, then retries |
| `Only N locations allowed at a time` | Waits 5 minutes, then retries |
| `Not authenticated`, `No such user` | *Needs attention*: check the API key |
| `Must be a customer` | *Needs attention*: premium subscription missing or expired |
| `Owner locked` | *Needs attention*: account locked |
| `Password not provided` / `Invalid password` | *Needs attention*: retry with the password |
| `Resource not found` | *Failed*: the file was deleted or the link is wrong |
| `Resource not allowed` | *Failed*: the owner restricted access |
| Server errors (5xx) | Retries with the normal backoff |
| Anything else | Waits 5 minutes, then retries |

Waiting counts against `downloads.max_retries`. A download that waits on rate
limits too often ends up *failed* and can be retried by hand.

## Not supported

- Free (non-premium) downloads. These need the website flow with wait timers,
  and 1fichier allows API downloads only for customers.
- Logging in with email and password (website cookies) as an alternative to
  the API key.
- 1fichier folder links.
- Downloading `.html` files from 1fichier. An HTML response is treated as an
  error page.
