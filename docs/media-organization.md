# Media organization

When a movie or a TV episode finishes downloading, Direkt renames it and moves
it into your media library, the way Jellyfin and Plex expect it, so a library
scan adds it with no manual steps.

```text
<movies>/Blade Runner 2049 (2017)/Blade Runner 2049 (2017).mkv
<tv>/The Office (2005)/Season 03/The Office (2005) - S03E10.mkv
<tv>/Show/Season 01/Show - S01E01-E02.mkv        (multi-episode file)
```

The naming rules come from the `arrangerr` script. Direkt doesn't need
arrangerr's cron schedule, file-age check or partial-file filter, because it
knows exactly when a transfer is complete.

## Setting it up

Set the folders under **Settings → Media library**, or with environment
variables. An environment variable takes precedence over the settings page and
locks the field there.

| Folder   | Environment variable  | Holds |
|----------|-----------------------|-------|
| Movies   | `DIREKT_MOVIES_DIR`   | `Title (Year)/Title (Year).ext` |
| TV       | `DIREKT_TV_DIR`       | `Show (Year)/Season NN/Show (Year) - SNNEMM.ext` |
| Fallback | `DIREKT_UNSORTED_DIR` | Videos that can't be identified, under their original name |

Each folder must be an absolute path to an existing directory that Direkt can
write to. The settings page checks this when you save. Movie, TV and fallback
folders must be separate, and none may be inside another. Point your Jellyfin
"Movies" and "Shows" libraries at the first two.

In Docker, these are paths **inside the container**. Mount your library into
the Direkt container, for example `- /srv/media:/media`, then use
`/media/movies` and `/media/tv`. When the library is on the same filesystem as
the download folder, a move is an instant rename. Otherwise Direkt copies the
file and then deletes the original.

When no movie or TV folder is set, downloads stay in the download folder, as
before.

## How a download is placed

Direkt uses:

- the **type** chosen on the confirmation menu (movie, TV series, …). A
  download submitted through the API without a type is classified from its
  file name.
- the **title and year** found by the TMDB lookup on the confirmation menu.
  For downloads that skipped it, Direkt looks the title up while organizing.
  Without a TMDB key, or when TMDB is down, it uses the title from the file
  name, title-cased.
- the **file name** for the season and episode numbers, and for the year when
  the lookup found none.

Placement is conservative. A file is never put into a library on a guess:

| Situation | Result |
|-----------|--------|
| Movie with a title and year | Movie library |
| Episode with a show, season and episode number | TV library |
| A "movie" without a year, a "movie" named `S01E01`, or a "series" without `SxxEyy` | Fallback folder, with a note. Without a fallback folder, the download is marked *Organizing failed* and stays put. |
| Type *Unsorted*, or an unrecognizable name | Fallback folder (same as above) |
| Game, music, software | Stays in the download folder |
| Archive (`.zip`, `.rar`, …) or another non-video file | Stays, with a note. Direkt does not extract archives. |
| The destination file already exists | *Organizing failed*: Direkt never replaces a library file. The download stays in the download folder. |
| The movie or TV library folder doesn't exist (for example, a missing mount) | *Organizing failed*. Direkt never creates the library folder itself, only the show, movie and season folders inside it. |

Titles are made safe as folder and file names. `Title: Subtitle` becomes
`Title - Subtitle`, and characters such as `/ \ * ? " < > |` and control or
invisible characters are removed. Direkt never creates a folder through a
symlink.

## States

Organizing is tracked separately from the transfer (`post_state`; see
[state-machine.md](state-machine.md)). A transfer that succeeded stays
`completed` whatever happens while organizing.

| `post_state` | Web UI | Meaning |
|--------------|--------|---------|
| `pending` | Organizing pending | Waiting its turn (one at a time) |
| `processing` | Organizing | Being moved |
| `completed` | Organized / Needs sorting | Moved. `file_path` is the new location. *Needs sorting* means it went to the fallback folder. |
| `skipped` | (no label) | Not organized; `post_error` may say why |
| `failed` | Organizing failed | `post_error` says why. The file is where it was. |

`post_error` holds the reason a download failed, was skipped, or went to the
fallback folder. After fixing the cause (setting a folder, freeing the name,
fixing a mount), click **Organize again** on the download, or call
`POST /api/v1/downloads/{id}/organize`. This works for `failed` and `skipped`
downloads.

Downloads completed before the libraries were set are skipped. Use
*Organize again* to move them.

## Recovery

Before moving, Direkt records the destination (`post_target`). If Direkt
stops in the middle of organizing:

- after restart the download is `pending` again and is redone.
- if the move finished but wasn't recorded, Direkt finds the file at the
  recorded destination (source gone, size matching) and marks it organized.
- a copy between filesystems writes to a hidden temporary file
  (`.<name>.direkt-partial`) next to the destination and only then gives it
  its final name. An interrupted copy never leaves a truncated video in the
  library. The next attempt replaces the leftover temporary file.

## Limitations

- Naming follows one fixed convention (the one Jellyfin and Plex recommend).
  It is not configurable yet.
- Archives are not extracted. Season packs shipped as `.zip`/`.rar` stay in
  the download folder.
- Subtitles and other companion files are not moved. Direkt downloads single
  files, so they are separate downloads.
- An organized file can be removed from the list, but Direkt won't delete it
  from the library.
