# 1fichier

Direkt downloads 1fichier files with a **premium account**. There are two ways
to connect it, both under **Settings → 1fichier**:

- **Account sign-in** with your 1fichier email and password. This works like
  a browser session and is the same as JDownloader's account login.
- **API key** for the official API, the same as JDownloader's "premium API"
  mode.

If both are set, downloads use the account.

## Account sign-in

1. Under **Settings → 1fichier → Account**, enter your email and password,
   then click **Sign in**.
2. If your account uses two-factor authentication, Direkt asks for the
   6-digit code. Enter it and click **Sign in** again within 10 minutes. This
   works for both authenticator-app and emailed codes.
3. Settings then shows the account and the end date of its premium
   subscription.

What Direkt sends and keeps:

- It posts the 1fichier login form with *long session* and *cookie for the
  other 1fichier domains*. It never sends *delete all old sessions*, so your
  browser sessions stay logged in.
- It stores the email, the password and the session cookie in its database
  (`<data_dir>/direkt.db`) in plain text. Storing the password lets Direkt sign
  in again by itself when 1fichier ends the session. The password is never
  shown again (the field displays `************************`), returned by the
  API, or logged. Protect `data_dir` and its backups accordingly.
- **Sign out** deletes the stored email, password and session.

When the session ends:

- Direkt signs in again with the stored credentials, once per download
  attempt. Only one worker at a time does this; the others reuse the new
  session.
- If 1fichier rejects the stored credentials, Direkt stops trying, so it
  doesn't trip 1fichier's lockout after repeated failures. Downloads wait in
  *needs attention*, and the Settings page asks you to sign in again.
- Accounts with two-factor authentication can't be signed in again
  automatically. Their downloads wait until you sign in again under Settings.

How a download works: Direkt opens the file page with the session. 1fichier
redirects premium accounts to the direct link, or answers the page's download
form (`did=0`, SSL download) with that redirect. For password-protected files,
Direkt submits the page's password form. Pages are requested in English, and
1fichier's error pages map to the same wait / needs attention / failed rules
as the API (see the table below). Direkt sends at most one request per second
to 1fichier, counting site and API requests together.

1fichier's API documentation advises against cookie-based sign-in in
programs. Sign-in therefore depends on 1fichier's web pages and can break
when they change. The API key is the more stable fallback.

## API key

1. Log in on 1fichier.com and copy your API key from
   *My account → Settings → API key*. 1fichier gives API keys only to
   customers (Premium, Premium Light, Premium GOLD, Access, CDN).
2. In Direkt, open **Settings → 1fichier → API key**, paste the key, and click
   **Save**. It takes effect on the next download attempt, with no restart
   needed. Once saved, the field shows only `************************`.

   Alternatively, set the `DIREKT_ONEFICHIER_API_KEY` environment variable.
   If it's set, it takes precedence, and the Settings field is shown locked.

Without a sign-in or a key, 1fichier links are still accepted, but they stop
in *needs attention* until you configure one and retry them.

## CDN credits

**Settings → 1fichier → CDN credits** shows the account's CDN credit
balance, whether the account is set to spend the credits on its own
downloads, where the figures came from, and when they were checked.

- With a signed-in account, Direkt reads them from the account's settings
  page, `/console/params.pl` (*"Your account has **… GB** of CDN credits"*
  and the *Use my credits for my own downloads* checkbox). This happens right
  after signing in.
- With only an API key, Direkt calls `user/info.cgi`, which reports `cdn`
  (credits in GB) and `use_cdn`. This happens right after the key is saved.
- **Refresh credits** reads them again. 1fichier allows `user/info.cgi` only
  once every 5 minutes per account (and once a minute per IP), so Direkt
  refuses a refresh within 5 minutes of the last check.
- Signing out or removing the key clears the figures that came from it.

Whether downloads spend credits is decided by the *Use my credits for my own
downloads* setting in your 1fichier account.

**Accounts with CDN credits but no subscription** are supported. Settings
shows "No subscription, downloads use CDN credits". In API-key mode, Direkt
asks `get_token.cgi` to spend credits (`"cdn": 1`) when the last check showed
the account uses them, as the API documentation describes for credit-based
downloads. If 1fichier refuses a download for lack of premium status, the
download waits in *needs attention* with a hint to check the *Use my
credits* setting.

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

## How the API mode works

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

- Free (non-premium) downloads. These need wait timers and are refused for
  API keys. A signed-in free account stops in *needs attention*.
- Automatic re-sign-in for accounts with two-factor authentication.
- Captchas on the sign-in page. JDownloader doesn't handle these either; if
  1fichier asks for one, sign-in fails with an error.
- 1fichier folder links.
- Downloading `.html` files from 1fichier. An HTML response is treated as an
  error page.
