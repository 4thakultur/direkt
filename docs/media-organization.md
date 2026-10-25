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

Set the folders with environment variables when Direkt is launched. For
safety, the web UI and the API cannot change them: nothing reachable over the
network can redirect where Direkt writes files. **Settings → Media library**
shows the folders and whether each one passed its startup check (it exists, is
a folder, and Direkt can write to it). Problems are also logged at startup.
Restart Direkt after changing a variable.

| Folder   | Environment variable  | Holds |
|----------|-----------------------|-------|
| Movies   | `DIREKT_MOVIES_DIR`   | `Title (Year)/Title (Year).ext` |
| TV       | `DIREKT_TV_DIR`       | `Show (Year)/Season NN/Show (Year) - SNNEMM.ext` |
| Fallback | `DIREKT_UNSORTED_DIR` | Videos that can't be identified, under their original name |

Each folder must be an absolute path to an existing directory that Direkt can
write to. Movie, TV and fallback folders must be separate, and none may be
inside another; Direkt refuses to start otherwise. Point your Jellyfin
"Movies" and "Shows" libraries at the first two.

In Docker, these are paths **inside the container**. The shipped
`compose.yaml` already sets them to `/media/movies`, `/media/tv` and
`/media/unsorted` and mounts a host folder at each; you choose the host
folders in `.env`, using their real (case-sensitive) names:

```sh
HOST_MOVIES_DIR=/srv/media/Movies
HOST_TV_DIR=/srv/media/TV Shows
HOST_UNSORTED_DIR=/srv/media/Unsorted
```

See [docker.md](docker.md#volumes). When the library is on the same
filesystem as the download folder and Direkt sees both through one mount, a
move is an instant rename. Otherwise, as with the separate mounts of the
shipped `compose.yaml`, Direkt copies the file and then deletes the original.

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
| Archive (`.zip`, `.rar`, `.7z`, split volumes) of a movie or series | Extracted; each video inside is placed by the rules above. See [Archives](#archives). |
| Disk image (`.iso`), `.gz`/`.xz` file, or another non-video file | Stays, with a note. |
| The destination file already exists | *Organizing failed*: Direkt never replaces a library file. The download stays in the download folder. |
| The movie or TV library folder doesn't exist (for example, a missing mount) | *Organizing failed*. Direkt never creates the library folder itself, only the show, movie and season folders inside it. |

Titles are made safe as folder and file names. `Title: Subtitle` becomes
`Title - Subtitle`, and characters such as `/ \ * ? " < > |` and control or
invisible characters are removed. Direkt never creates a folder through a
symlink.

## Archives

Season packs and some movies come as archives. Direkt extracts `.zip`,
`.rar`, `.7z` and `.tar` archives, and archives split into volumes
(`.part1.rar`, `.part2.rar`, …; `.rar` + `.r00`, `.r01`, …; `.zip` + `.z01`;
`.001`, `.002`, …), with 7-Zip. The Docker image includes it (Debian's
`7zip`, plus `7zip-rar` from non-free for RAR). Outside Docker, install it
yourself so that `7z` (or `7zz`) is on the `PATH`. Without it, archives stay
in the download folder with a note, and Direkt logs a warning at startup.

For an archive of a movie or a series (chosen on the confirmation menu, or
recognized from the name):

1. Direkt lists the archive first and refuses to extract one with absolute
   paths, `..`, links, encrypted (password-protected) entries or more than
   10,000 files. It also checks that the download folder has room for the
   extracted files.
2. It extracts into a private hidden folder next to the archive
   (`.direkt-extract-<id>`), so moving the videos into a library on the same
   filesystem is a rename.
3. Each video inside, except samples (`Sample/` folders, `*sample*` files),
   is placed by the rules above, with the title from the confirmation menu.
   Episodes named only `S01E03.mkv` get the show's name from the archive's
   name. Videos that can't be identified go to the fallback folder. Other
   files (`.nfo`, subtitles, images) are not kept.
4. The temporary folder is removed. By default **the archive stays in the
   download folder**: it is the download's file, so the trash button on the
   download deletes it (the extracted videos stay in the library).

### Deleting archives after extraction

Turn on **Settings → Media library → Delete archives after extraction** (or
`PATCH /api/v1/settings/misc` with `"delete_extracted_archives": true`) to
have Direkt delete an archive once it is extracted. It is off by default and
applies from the next extraction.

- The archive is deleted only when **every** video in it was placed (in the
  library or the fallback folder). When one could not be, the archive stays
  so that *Organize again* can retry.
- All the parts of a split archive are deleted (the files in the download
  folder named as its volumes), not only the first.
- The deletion happens after the result is recorded. The download then
  points to where its videos went: the movie file, or the folder that holds
  the episodes (for example the season folder). If Direkt stops in between,
  the archive is simply kept.
- The downloads of the other volumes stay in the list; their files are gone,
  so remove them from the list.
- Only regular files directly in the download folder are deleted.

When some videos can't be placed (for example, the episode is already in
the library), the download is *Organizing failed* with the reason; the others
are already in the library. *Organize again* redoes the extraction and skips
the videos already placed (same name and size in the library).

A split archive is usually downloaded as one download per volume. Later
volumes say which first volume they belong to. The first volume's download
extracts the set: if some volumes are still downloading, it waits (skipped,
"waiting for the other parts"), and the last volume to finish queues it
again. Volumes must keep their names, in the same folder.

Archives of games, music and software are never extracted.

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
fallback folder. After fixing the cause (setting a folder and restarting,
freeing the name, fixing a mount), click **Organize again** on the download, or call
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
- an interrupted extraction is redone from the start: 7-Zip is stopped on
  shutdown, the leftover `.direkt-extract-<id>` folder is cleared, and
  videos that already reached the library are recognized by name and size.

## Limitations

- Naming follows one fixed convention (the one Jellyfin and Plex recommend).
  It is not configurable yet.
- Password-protected archives are not extracted. Nested archives (an
  archive inside the archive) are not extracted either.
- Unless archives are deleted after extraction, an extracted archive is
  kept in the download folder until you delete it. Either way a season pack
  needs room for both the archive and its videos while it is extracted.
- Subtitles and other files inside an archive are not moved with the
  videos.
- Subtitles and other companion files downloaded separately are not moved.
  Direkt downloads single files, so they are separate downloads.
- An organized file can be removed from the list, but Direkt won't delete it
  from the library.
