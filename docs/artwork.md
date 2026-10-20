# Artwork

The dashboard shows cover artwork for movies and TV shows, found from each
download's file name. Downloads without a confident match keep their
file-type tile (`ZIP`, `MKV`, …).

## Setup

Artwork needs a free API key from [The Movie Database](https://www.themoviedb.org/)
(TMDB): create an account, open **Settings → API**, and paste either the
**API key** or the **API read access token** into Direkt under
**Settings → Artwork**. You can set it with the `DIREKT_TMDB_API_KEY`
environment variable instead, which then takes precedence and locks the field.
Without a key, Direkt makes no requests to TMDB.

## How a match is found

1. The file name is parsed, for example
   `Cyberpunk.Edgerunners.S02.MULTi.1080p.NF.WEB.x264-BYOR.zip` becomes the TV
   show "Cyberpunk Edgerunners", season 2, and
   `Blade.Runner.2049.2017.1080p.BluRay.mkv` becomes the movie
   "Blade Runner 2049" (2017). Names without an episode marker, a release year,
   or a release tag such as `1080p` or `WEB` are not looked up, so ordinary
   files (`backup.zip`, `ubuntu-24.04.iso`) are left alone.
2. TMDB is searched for that title (and year, if any).
3. A result counts only if its title or original title matches exactly once
   punctuation and case are ignored (`Cyberpunk: Edgerunners` matches) and,
   when the name has a year, the release year is within one year. Otherwise
   there is no artwork. Direkt never guesses with a loose match.
4. The wide backdrop is used if TMDB has one, else the poster.

Episodes of one show share the show's artwork.

## Privacy and caching

- Only the parsed title and year are sent to TMDB, never the download URL.
- Direkt downloads the image itself and caches it in `<data_dir>/artwork`.
  Your browser loads it from Direkt (`/artwork/<id>`, login required), so it
  never contacts TMDB, and the Content-Security-Policy stays `img-src 'self'`.
- Lookups run in one background worker and never slow down the page. Names
  with no match are looked up again after a day, and after network errors
  after 10 minutes. Saving a new key retries them at once.
- Deleting `<data_dir>/artwork` is safe: images are fetched again when needed.
  Removing the key keeps cached images.
