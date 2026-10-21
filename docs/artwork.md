# Confirming a download: type, title and artwork

Clicking **Download** doesn't queue the link right away. Direkt first looks it
up and opens a confirmation menu showing:

- **Provider**: 1fichier, or "Direct link" for ordinary HTTP/HTTPS links.
- **Name**: the title found by the metadata lookup (for example
  "Cyberpunk: Edgerunners (2022)"), with the file name under it.
- **Artwork**, or the file-type tile (`ZIP`, `MKV`, …) when none is found.
- **Size**: exact for direct links; about the size 1fichier displays (≈) for
  1fichier.
- **Type**: Movie, TV series, Video game, Music, Software or Unsorted,
  pre-filled with what Direkt detected (marked "detected"). Changing it looks
  the title and artwork up again for that type.

**Start download** queues it with the chosen type; **Cancel** closes the menu.
The type is stored with the download, so media organization can use it.

## Where the details come from

| Detail | Direct link | 1fichier |
|--------|-------------|----------|
| Name and size | A request for the first byte only (`Range: bytes=0-0`): `Content-Disposition` or the URL for the name, `Content-Range` or `Content-Length` for the size | The file's public page, fetched without signing in (`tier-name` and `tier-feat`), spaced like every other 1fichier request. `file/info.cgi` is never used: it is known to get IP addresses banned. |
| Title and artwork | Movies and TV series: [TMDB](https://www.themoviedb.org/) (needs a free API key). Video games: the Steam store search (no key). Music, Software, Unsorted: none. | same |

If the link is gone (HTTP 404 or 410, or "file not found" on 1fichier) or
forbidden by the network policy, Direkt shows the error instead of the menu.
Other problems (a timeout, a rate limit) are shown as notes in the menu, and
you can still start the download.

## Setting up TMDB

Movie and series artwork needs a free TMDB API key: create an account, open
**Settings → API**, and paste either the **API key** or the **API read access
token** into Direkt under **Settings → Artwork**. Alternatively, set the
`DIREKT_TMDB_API_KEY` environment variable, which takes precedence and locks
the field. Without a key, the menu says so and shows the file-type tile.

## How the type is detected

From the file name, in this order:

1. Video extensions (`.mkv`, `.mp4`, …): a TV series if there's an episode or
   season marker (`S02`, `S03E10`, `2x05`, `Season 2`), otherwise a movie if
   there's a release year or a release tag (`1080p`, `WEB`, `BluRay`, …).
2. Audio extensions (`.mp3`, `.flac`, …): Music.
3. A game group or repacker (`CODEX`, `TENOKE`, `FitGirl`, `GOG`, …) or a
   Switch file (`.nsp`, `.xci`): Video game. The title stops at the version,
   build or group: `Hades.II-TENOKE.zip` is "Hades II".
4. Installers (`.exe`, `.msi`, `.dmg`, `.deb`, …): Software.
5. Episode markers, then audio tags (`FLAC`, `MP3`, `320kbps`) without
   video tags, then a year or release tag, then platform words (`x64`,
   `amd64`, `Portable`, …).
6. Anything else: Unsorted.

## Matching is conservative

A search result counts only if its title (or original title) matches the
name's title, ignoring case, accents, punctuation and spacing
(`Cyberpunk: Edgerunners` matches, `Hades™ II` matches "Hades II",
`L_ère.d_Ultron` matches "L'Ère d'Ultron"), and, when the name has a year, the
release year is within one year. When the name has a year, a leading article
may also be missing: `Avengers.2012` matches "The Avengers" (2012). An exact
match always wins over one that needs the article dropped.

Release names often carry a localized title while TMDB answers with the
English one: `Avengers.L_ère.d_Ultron.2015` finds "Avengers: Age of Ultron".
When no result's title matches, Direkt checks the translated titles of the
first three results (one extra request each) and uses the translation that
matches, so this shows as "Avengers : L'Ère d'Ultron" and media organization
files it under that name.

Otherwise the menu shows no artwork rather than a wrong one. Episodes of one
show share the show's artwork.

## Privacy and caching

- TMDB receives only the parsed title and year (and, for the translation
  check, the ids of its own search results); Steam only the parsed game
  title. The download link and password are never sent to them.
- Direkt downloads the image itself and caches it, with its title, in
  `<data_dir>/artwork`. Your browser loads it from Direkt (`/artwork/<id>`,
  login required), so it never contacts TMDB or Steam, and the
  Content-Security-Policy stays `img-src 'self'`.
- Titles with no match are looked up again after a day; after errors, after
  10 minutes. Saving a new TMDB key retries at once.
- Deleting `<data_dir>/artwork` is safe; downloads confirmed earlier then show
  their file-type tile.
- A confirmation, including the file password, stays in memory for an hour
  and is used once. The password is never sent back to the browser.
