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
/// it. Otherwise it falls back to copying into a newly created file and then
/// removing `src`. The copy also covers moves between filesystems. If
/// `dst` exists, fails with `AlreadyExists` and leaves both files untouched.
pub fn move_no_clobber(src: &Path, dst: &Path) -> io::Result<()> {
    match rename_no_replace(src, dst) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(e),
        Err(e) if is_rename_unsupported(&e) => {}
        Err(e) => return Err(e),
    }
    copy_no_clobber(src, dst)?;
    fs::remove_file(src)
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

/// Bytes available to unprivileged users on the filesystem containing `path`.
pub fn available_space(path: &Path) -> io::Result<u64> {
    fs4::available_space(path)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn reports_available_space() {
        let dir = tempfile::tempdir().unwrap();
        assert!(available_space(dir.path()).unwrap() > 0);
    }
}
