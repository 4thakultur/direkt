//! Filesystem helpers: untrusted-filename sanitization, collision-free
//! destinations, and moves that never overwrite existing files.
//!
//! All functions here are blocking; call them from `spawn_blocking` when they
//! may touch large files (cross-filesystem moves copy data).

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Maximum filename length in bytes (most filesystems allow 255).
const MAX_FILENAME_BYTES: usize = 200;

/// Turn an untrusted filename (from a server header or URL) into a single,
/// safe path component. Returns `None` if nothing usable remains.
///
/// Removes path separators, control characters and characters that are
/// invalid on common filesystems. Strips leading dots, so the result cannot
/// be `.`, `..` or a hidden file. Truncates over-long names but keeps the
/// extension.
pub fn sanitize_filename(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|&c| !is_invisible_or_bidi(c))
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned
        .trim()
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' ']);
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '_') {
        return None;
    }
    Some(truncate_keep_extension(trimmed, MAX_FILENAME_BYTES))
}

/// Invisible and bidirectional-control characters. They can disguise a name,
/// e.g. U+202E (right-to-left override) makes `report\u{202E}vkm.exe` display
/// as `reportexe.mkv`.
fn is_invisible_or_bidi(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'                 // soft hyphen
            | '\u{061C}'           // Arabic letter mark
            | '\u{180E}'           // Mongolian vowel separator
            | '\u{200B}'..='\u{200F}' // zero-width space/joiners, LRM, RLM
            | '\u{202A}'..='\u{202E}' // bidi embeddings and overrides
            | '\u{2060}'..='\u{2064}' // word joiner, invisible operators
            | '\u{2066}'..='\u{2069}' // bidi isolates
            | '\u{FEFF}'           // zero-width no-break space / BOM
    )
}

fn truncate_keep_extension(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_owned();
    }
    let (stem, ext) = split_extension(name);
    // Keep short extensions only; a 100-byte "extension" is not one.
    let ext = if ext.len() <= 16 { ext } else { "" };
    let budget = max - ext.len();
    let mut cut = budget.min(stem.len());
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{}", stem[..cut].trim_end(), ext)
}

/// Split `name.ext` into (`name`, `.ext`). Names without a dot, or with only a
/// leading dot, have no extension.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// Pick a path in `dir` for `filename` that does not exist yet, appending
/// ` (1)`, ` (2)`, … before the extension on collision. Dangling symlinks count
/// as existing.
pub fn unique_destination(dir: &Path, filename: &str) -> io::Result<PathBuf> {
    let candidate = dir.join(filename);
    if !exists_no_follow(&candidate)? {
        return Ok(candidate);
    }
    let (stem, ext) = split_extension(filename);
    for n in 1..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !exists_no_follow(&candidate)? {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many files with the same name",
    ))
}

fn exists_no_follow(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Move `src` to `dst` without ever replacing an existing `dst`.
///
/// Uses an atomic no-replace rename when the platform and filesystem support
/// it. Otherwise (for example between filesystems) it copies `src` next to
/// `dst` under a temporary name, links the copy into place, and then removes
/// `src`. If `dst` exists, fails with `AlreadyExists` and leaves both files
/// untouched.
pub fn move_no_clobber(src: &Path, dst: &Path) -> io::Result<()> {
    match rename_no_replace(src, dst) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(e),
        Err(e) if is_rename_unsupported(&e) => {}
        Err(e) => return Err(e),
    }
    copy_then_link(src, dst)?;
    fs::remove_file(src)
}

/// Copy `src` to `dst` (which must not exist) through a temporary file next
/// to it, so an interrupted copy never leaves a partial file at `dst`. A
/// temporary file left by an interrupted earlier attempt is replaced.
fn copy_then_link(src: &Path, dst: &Path) -> io::Result<()> {
    if exists_no_follow(dst)? {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    }
    let tmp = temp_sibling(dst)?;
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    copy_no_clobber(src, &tmp)?;
    let linked = match rename_no_replace(&tmp, dst) {
        Ok(()) => return Ok(()),
        Err(e) if is_rename_unsupported(&e) => match fs::hard_link(&tmp, dst) {
            // No hard links either (e.g. some network or FAT filesystems):
            // copy into a newly created `dst` as a last resort.
            Err(e) if is_link_unsupported(&e) => copy_no_clobber(&tmp, dst),
            other => other,
        },
        Err(e) => Err(e),
    };
    let _ = fs::remove_file(&tmp);
    linked
}

/// `.<name>.direkt-partial` next to `path`: hidden, and without a media
/// extension, so media servers ignore it.
fn temp_sibling(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a file path"))?;
    let mut tmp = std::ffi::OsString::from(".");
    tmp.push(name);
    tmp.push(".direkt-partial");
    Ok(path.with_file_name(tmp))
}

fn is_link_unsupported(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::Unsupported | io::ErrorKind::PermissionDenied
    )
}

/// Create the directories `components` below the existing directory `root`,
/// returning the deepest one. Existing directories are reused; a component
/// that exists as anything else, including a symlink, is refused with
/// `AlreadyExists`, so files are never placed through a link.
pub fn create_dirs_in(root: &Path, components: &[&str]) -> io::Result<PathBuf> {
    let mut dir = root.to_path_buf();
    for component in components {
        if Path::new(component).components().count() != 1 || matches!(*component, "" | "." | "..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a single path component",
            ));
        }
        dir.push(component);
        if let Err(e) = fs::create_dir(&dir)
            && e.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(e);
        }
        if !fs::symlink_metadata(&dir)?.file_type().is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a folder", dir.display()),
            ));
        }
    }
    Ok(dir)
}

#[cfg(target_os = "linux")]
fn rename_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, src, CWD, dst, RenameFlags::NOREPLACE).map_err(io::Error::from)
}

#[cfg(not(target_os = "linux"))]
fn rename_no_replace(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

fn is_rename_unsupported(e: &io::Error) -> bool {
    // EXDEV: different filesystems. EINVAL/ENOSYS/Unsupported: the filesystem or
    // kernel lacks RENAME_NOREPLACE (e.g. some network filesystems).
    matches!(
        e.kind(),
        io::ErrorKind::CrossesDevices | io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
    ) || e.raw_os_error() == Some(38) // ENOSYS
}

/// Copy `src` into a newly created `dst` (fails if `dst` exists) and fsync it.
/// A partially written `dst` is removed on error.
pub fn copy_no_clobber(src: &Path, dst: &Path) -> io::Result<()> {
    let mut reader = File::open(src)?;
    let mut writer = OpenOptions::new().write(true).create_new(true).open(dst)?;
    let result = io::copy(&mut reader, &mut writer).and_then(|_| writer.sync_all());
    if let Err(e) = result {
        drop(writer);
        let _ = fs::remove_file(dst);
        return Err(e);
    }
    Ok(())
}

/// Restrict a private directory to its owner (mode 0700), creating it if
/// needed. Used for the data directory, which holds credentials.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Make `path` readable only by its owner (mode 0600), creating it empty if
/// it does not exist, so it is never world-readable even briefly. Existing
/// companions with the given suffixes (e.g. SQLite's `-wal`, `-shm`) are
/// restricted too.
pub fn create_private_file(path: &Path, companions: &[&str]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    drop(options.open(path)?);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let private = || fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, private())?;
        for suffix in companions {
            let mut companion = path.as_os_str().to_owned();
            companion.push(suffix);
            match fs::set_permissions(Path::new(&companion), private()) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(())
}

/// Delete `path` if it is a regular file directly inside `dir`. Symlinks,
/// directories and anything elsewhere are refused with `PermissionDenied`, so
/// a stale or tampered path can never remove files outside `dir`. Returns
/// `false` if the file was already gone.
pub fn remove_file_in(dir: &Path, path: &Path) -> io::Result<bool> {
    let refuse = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            why.to_owned(),
        ))
    };
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return refuse("not a file path");
    };
    let parent = match parent.canonicalize() {
        Ok(parent) => parent,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if parent != dir.canonicalize()? {
        return refuse("outside the download directory");
    }
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => return refuse("not a regular file"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    }
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Bytes available to unprivileged users on the filesystem containing `path`.
pub fn available_space(path: &Path) -> io::Result<u64> {
    fs4::available_space(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_regular_files_inside_the_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("downloads");
        fs::create_dir(&dir).unwrap();
        let outside = root.path().join("outside.txt");
        fs::write(&outside, "keep").unwrap();
        let inside = dir.join("movie.mkv");
        fs::write(&inside, "data").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/nested.mkv"), "data").unwrap();

        let denied = |p: &Path| remove_file_in(&dir, p).unwrap_err().kind();
        assert_eq!(denied(&outside), io::ErrorKind::PermissionDenied);
        assert_eq!(
            denied(&dir.join("../outside.txt")),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            denied(&dir.join("sub/nested.mkv")),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(denied(&dir.join("sub")), io::ErrorKind::PermissionDenied);
        #[cfg(unix)]
        {
            let link = dir.join("link.mkv");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert_eq!(denied(&link), io::ErrorKind::PermissionDenied);
            assert!(link.exists());
        }
        assert!(outside.exists());

        assert!(remove_file_in(&dir, &inside).unwrap());
        assert!(!inside.exists());
        assert!(!remove_file_in(&dir, &inside).unwrap(), "already gone");
    }

    #[test]
    fn sanitizes_hostile_names() {
        let cases = [
            ("movie.mkv", Some("movie.mkv")),
            ("../../etc/passwd", Some("_.._etc_passwd")),
            ("..", None),
            (".", None),
            ("", None),
            ("   ", None),
            ("/", None),
            (".bashrc", Some("bashrc")),
            ("a\\b\\c.txt", Some("a_b_c.txt")),
            ("C:\\Windows\\x.exe", Some("C__Windows_x.exe")),
            ("name\0with\nnul.bin", Some("name_with_nul.bin")),
            ("trailing. . ", Some("trailing")),
            (
                "Show S01E02 – Ünïcödé.mkv",
                Some("Show S01E02 – Ünïcödé.mkv"),
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(sanitize_filename(raw).as_deref(), expected, "{raw:?}");
        }
    }

    #[test]
    fn sanitized_names_are_single_components() {
        for raw in ["../x", "a/../../b", "..\\..\\x", "x/.", "./."] {
            if let Some(name) = sanitize_filename(raw) {
                let path = Path::new(&name);
                assert_eq!(path.components().count(), 1, "{raw:?} -> {name:?}");
                assert!(!name.contains('/') && !name.contains('\\'));
                assert_ne!(name, "..");
            }
        }
    }

    #[test]
    fn truncates_long_names_keeping_extension() {
        let long = format!("{}.mkv", "a".repeat(400));
        let name = sanitize_filename(&long).unwrap();
        assert!(name.len() <= MAX_FILENAME_BYTES);
        assert!(name.ends_with(".mkv"));

        let multibyte = format!("{}.mkv", "é".repeat(300));
        let name = sanitize_filename(&multibyte).unwrap();
        assert!(name.len() <= MAX_FILENAME_BYTES);
        assert!(name.ends_with(".mkv"));
    }

    #[test]
    fn unique_destination_avoids_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let first = unique_destination(dir.path(), "file.mkv").unwrap();
        assert_eq!(first, dir.path().join("file.mkv"));
        fs::write(&first, b"x").unwrap();
        let second = unique_destination(dir.path(), "file.mkv").unwrap();
        assert_eq!(second, dir.path().join("file (1).mkv"));
        fs::write(&second, b"x").unwrap();
        assert_eq!(
            unique_destination(dir.path(), "file.mkv").unwrap(),
            dir.path().join("file (2).mkv")
        );
        assert_eq!(
            unique_destination(dir.path(), "noext").unwrap(),
            dir.path().join("noext")
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_counts_as_taken() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/nonexistent/target", dir.path().join("x.bin")).unwrap();
        assert_eq!(
            unique_destination(dir.path(), "x.bin").unwrap(),
            dir.path().join("x (1).bin")
        );
    }

    #[test]
    fn move_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::write(&src, b"new").unwrap();
        fs::write(&dst, b"old").unwrap();
        let err = move_no_clobber(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&dst).unwrap(), b"old");
        assert_eq!(fs::read(&src).unwrap(), b"new");

        fs::remove_file(&dst).unwrap();
        move_no_clobber(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"new");
        assert!(!src.exists());
    }

    #[cfg(unix)]
    #[test]
    fn move_does_not_follow_symlink_at_destination() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"precious").unwrap();
        let src = dir.path().join("src");
        fs::write(&src, b"attacker").unwrap();
        let dst = dir.path().join("dst");
        std::os::unix::fs::symlink(&victim, &dst).unwrap();
        assert!(move_no_clobber(&src, &dst).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
    }

    #[test]
    fn copy_then_link_uses_a_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.mkv");
        let dst = dir.path().join("dst.mkv");
        fs::write(&src, b"movie").unwrap();
        // Left behind by an interrupted copy.
        let tmp = dir.path().join(".dst.mkv.direkt-partial");
        fs::write(&tmp, b"mo").unwrap();

        copy_then_link(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"movie");
        assert!(!tmp.exists(), "the temporary file is cleaned up");

        fs::write(&src, b"other").unwrap();
        let err = copy_then_link(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&dst).unwrap(), b"movie");
        assert!(!tmp.exists());
    }

    #[test]
    fn library_folders_are_created_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let season = create_dirs_in(root, &["Show (2020)", "Season 01"]).unwrap();
        assert_eq!(season, root.join("Show (2020)/Season 01"));
        assert!(season.is_dir());
        // Existing folders are reused.
        assert_eq!(
            create_dirs_in(root, &["Show (2020)", "Season 01"]).unwrap(),
            season
        );

        fs::write(root.join("file"), b"x").unwrap();
        assert_eq!(
            create_dirs_in(root, &["file", "Season 01"])
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        for bad in ["..", ".", "", "a/b"] {
            assert_eq!(
                create_dirs_in(root, &[bad]).unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{bad:?}"
            );
        }
        #[cfg(unix)]
        {
            let elsewhere = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(elsewhere.path(), root.join("Linked")).unwrap();
            assert_eq!(
                create_dirs_in(root, &["Linked", "Season 01"])
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::AlreadyExists
            );
            assert!(!elsewhere.path().join("Season 01").exists());
        }
    }

    #[test]
    fn copy_fallback_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::write(&src, b"data").unwrap();
        copy_no_clobber(&src, &dst).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), b"data");
        let err = copy_no_clobber(&src, &dst).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[cfg(unix)]
    #[test]
    fn private_files_and_dirs_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&data).unwrap();
        assert_eq!(
            fs::metadata(&data).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let db = data.join("x.db");
        fs::write(&db, b"existing").unwrap();
        fs::set_permissions(&db, fs::Permissions::from_mode(0o644)).unwrap();
        let wal = data.join("x.db-wal");
        fs::write(&wal, b"").unwrap();
        fs::set_permissions(&wal, fs::Permissions::from_mode(0o644)).unwrap();
        create_private_file(&db, &["-wal", "-shm"]).unwrap();
        for f in [&db, &wal] {
            assert_eq!(fs::metadata(f).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(fs::read(&db).unwrap(), b"existing", "content is kept");

        let fresh = data.join("new.db");
        create_private_file(&fresh, &[]).unwrap();
        assert_eq!(
            fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn reports_available_space() {
        let dir = tempfile::tempdir().unwrap();
        assert!(available_space(dir.path()).unwrap() > 0);
    }
}
