//! Archive extraction with 7-Zip.
//!
//! Season packs and some movies come as `.zip`, `.rar` or `.7z` archives,
//! sometimes split into volumes that are downloaded separately
//! (`Show.S01.part1.rar`, `Show.S01.part2.rar`, …). The organizer extracts
//! them with the `7z` command (Debian's `7zip` package, plus `7zip-rar` for
//! RAR) and organizes the videos inside.
//!
//! Archives are untrusted. Before anything is written, the listing is
//! checked: no absolute paths, no `..`, no links, no encrypted entries, a
//! bounded number of entries, and their total size for the disk-space check.
//! Extraction then goes into a fresh private folder, and only regular files
//! found there are used.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// The 7-Zip commands looked for, in order. Debian's `7z` loads the RAR
/// codec of `7zip-rar`; `7zz` is the upstream build, which has it built in.
const PROGRAMS: &[&str] = &["7z", "7zz"];
/// Most entries an archive may hold.
const MAX_ENTRIES: usize = 10_000;
/// Most bytes of a listing line, and of error output kept.
const MAX_LINE: usize = 8 * 1024;
const MAX_STDERR: usize = 64 * 1024;
/// A password no archive uses. With it, 7-Zip reports an encrypted archive
/// instead of asking for a password.
const NO_PASSWORD: &str = "-pdirekt-no-password";

/// How a file takes part in an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// A whole archive, or the first volume of a split one: extract this.
    First,
    /// A later volume. `first` is the first volume's file name, in the same
    /// folder.
    Volume { first: String },
}

/// The role of `file_name` in an archive, if it is one Direkt extracts:
/// `.zip`, `.rar`, `.7z`, `.tar`, their volumes (`.partN.rar`, `.rNN`,
/// `.zNN`) and numbered splits (`.001`). Disk images and single-file
/// compression (`.iso`, `.gz`, …) are not extracted.
pub fn role(file_name: &str) -> Option<Role> {
    let (stem, ext) = file_name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    let lower = ext.to_ascii_lowercase();
    let digits = |s: &str, len: std::ops::RangeInclusive<usize>| {
        len.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
    };
    // Keep the case of the volume's own extension: `.R00` goes with `.RAR`.
    let ext_like = |new: &str| {
        if ext.bytes().any(|b| b.is_ascii_uppercase()) {
            new.to_ascii_uppercase()
        } else {
            new.to_owned()
        }
    };
    let volume = |first: String| Some(Role::Volume { first });
    match lower.as_str() {
        "rar" => match part_number(stem) {
            Some((base, number)) if number.trim_start_matches('0') != "1" => {
                let one = format!("{:0>width$}", 1, width = number.len());
                let part = &stem[base.len() + 1..][..4];
                volume(format!("{base}.{part}{one}.{ext}"))
            }
            _ => Some(Role::First),
        },
        "zip" | "7z" | "tar" => Some(Role::First),
        _ if lower.starts_with('r') && digits(&lower[1..], 2..=3) => {
            volume(format!("{stem}.{}", ext_like("rar")))
        }
        _ if lower.starts_with('z') && digits(&lower[1..], 2..=3) => {
            volume(format!("{stem}.{}", ext_like("zip")))
        }
        // Numbered splits of a file: `Movie.mkv.001`, `Show.7z.002`.
        _ if digits(&lower, 3..=3) && stem.contains('.') => {
            if lower == "001" {
                Some(Role::First)
            } else {
                volume(format!("{stem}.001"))
            }
        }
        _ => None,
    }
}

/// Whether `file_name` is part of the archive whose first volume is named
/// `first`: that file itself, or one of its later volumes.
pub fn in_set(file_name: &str, first: &str) -> bool {
    file_name == first || matches!(role(file_name), Some(Role::Volume { first: f }) if f == first)
}

/// `Name.part02` -> (`Name`, `02`).
fn part_number(stem: &str) -> Option<(&str, &str)> {
    let (base, last) = stem.rsplit_once('.')?;
    let number = last
        .get(..4)
        .filter(|p| p.eq_ignore_ascii_case("part"))
        .map(|_| &last[4..])?;
    (!base.is_empty()
        && (1..=4).contains(&number.len())
        && number.bytes().all(|b| b.is_ascii_digit()))
    .then_some((base, number))
}

/// Whether a file inside an archive is a release sample: named `sample`, or
/// in a `Sample` folder. Samples are not organized.
pub fn is_sample(path: &str) -> bool {
    let mut parts = path.split(['/', '\\']).rev();
    let file = parts.next().unwrap_or_default();
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    let is_word = |s: &str| s.eq_ignore_ascii_case("sample") || s.eq_ignore_ascii_case("samples");
    stem.split(|c: char| !c.is_alphanumeric()).any(is_word) || parts.any(is_word)
}

/// One file or folder listed in an archive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    encrypted: bool,
    link: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("other parts of the archive are missing")]
    MissingVolume,
    #[error("the archive is password-protected")]
    Encrypted,
    #[error("the archive contains an unsafe entry ({0})")]
    Unsafe(String),
    #[error("the archive holds more than {MAX_ENTRIES} files")]
    TooManyEntries,
    #[error("7-Zip could not read the archive: {0}")]
    Unreadable(String),
    #[error("could not run 7-Zip: {0}")]
    Io(#[from] io::Error),
    #[error("interrupted")]
    Cancelled,
}

/// The `7z` command.
#[derive(Debug, Clone)]
pub struct SevenZip {
    program: PathBuf,
}

impl SevenZip {
    /// Find 7-Zip on the `PATH`.
    pub fn find() -> Option<Self> {
        let path = std::env::var_os("PATH")?;
        PROGRAMS.iter().find_map(|name| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|p| is_executable(p))
                .map(Self::at)
        })
    }

    pub fn at(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// List `archive` and check that it is safe to extract.
    pub async fn list(
        &self,
        archive: &Path,
        cancel: &CancellationToken,
    ) -> Result<Vec<Entry>, ArchiveError> {
        let mut command = self.command();
        command
            .args(["l", "-slt", "-ba", "-sccUTF-8", NO_PASSWORD, "--"])
            .arg(archive);
        let mut entries: Vec<Entry> = Vec::new();
        let mut current: Option<Entry> = None;
        let status = run(command, cancel, |line| {
            let Some((key, value)) = line.split_once(" = ").or_else(|| {
                // `Key =` with an empty value.
                line.strip_suffix(" =").map(|key| (key, ""))
            }) else {
                return Ok(());
            };
            if key == "Path" {
                if let Some(done) = current.take() {
                    entries.push(done);
                }
                if entries.len() >= MAX_ENTRIES {
                    return Err(ArchiveError::TooManyEntries);
                }
                current = Some(Entry {
                    path: value.to_owned(),
                    ..Default::default()
                });
                return Ok(());
            }
            let Some(entry) = current.as_mut() else {
                return Ok(());
            };
            match key {
                "Size" => entry.size = value.parse().unwrap_or(0),
                "Folder" => entry.is_dir |= value == "+",
                "Encrypted" => entry.encrypted |= value == "+",
                // `D drwxr-xr-x`, `A lrwxrwxrwx`: DOS flags, then a Unix mode.
                "Attributes" => {
                    let mut words = value.split_whitespace().peekable();
                    let dos = words.next_if(|w| w.bytes().all(|b| b.is_ascii_uppercase()));
                    let mode = words.next().unwrap_or_default();
                    entry.is_dir |= dos.is_some_and(|w| w.contains('D')) || mode.starts_with('d');
                    entry.link |= mode.starts_with('l');
                }
                "Symbolic Link" | "Hard Link" => entry.link |= !value.is_empty(),
                _ => {}
            }
            Ok(())
        })
        .await?;
        if let Some(done) = current {
            entries.push(done);
        }
        status.check(archive)?;
        for entry in &entries {
            check_entry(entry)?;
        }
        Ok(entries)
    }

    /// Extract `archive` into the empty folder `into`.
    pub async fn extract(
        &self,
        archive: &Path,
        into: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), ArchiveError> {
        let mut command = self.command();
        let mut out = std::ffi::OsString::from("-o");
        out.push(into);
        command
            .args(["x", "-y", "-bso0", "-bsp0", "-sccUTF-8", NO_PASSWORD])
            .arg(out)
            .arg("--")
            .arg(archive);
        run(command, cancel, |_| Ok(())).await?.check(archive)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    meta.is_file()
}

/// Refuse entries that could write outside the extraction folder, or that
/// can't be extracted without a password.
fn check_entry(entry: &Entry) -> Result<(), ArchiveError> {
    let unsafe_entry = |why: &str| Err(ArchiveError::Unsafe(format!("{why}: {}", entry.path)));
    if entry.encrypted {
        return Err(ArchiveError::Encrypted);
    }
    if entry.link {
        return unsafe_entry("a link");
    }
    let path = entry.path.replace('\\', "/");
    if path.starts_with('/') || path.as_bytes().get(1) == Some(&b':') {
        return unsafe_entry("an absolute path");
    }
    if path.split('/').any(|part| part == "..") {
        return unsafe_entry("a path leaving the archive");
    }
    if path.chars().any(char::is_control) {
        return unsafe_entry("a control character in a name");
    }
    Ok(())
}

/// How 7-Zip ended, with what it wrote to standard error.
struct Status {
    code: Option<i32>,
    stderr: String,
}

impl Status {
    fn check(self, archive: &Path) -> Result<(), ArchiveError> {
        // 0 is success and 1 a warning; 7-Zip also reports some errors (a
        // missing volume) on stderr with 0.
        let failed = !matches!(self.code, Some(0 | 1)) || self.stderr.contains("ERROR");
        if !failed {
            return Ok(());
        }
        let text = self.stderr.to_ascii_lowercase();
        if text.contains("wrong password") || text.contains("encrypted") {
            return Err(ArchiveError::Encrypted);
        }
        if text.contains("missing volume")
            || (is_split_name(archive) && text.contains("unexpected end of archive"))
        {
            return Err(ArchiveError::MissingVolume);
        }
        Err(ArchiveError::Unreadable(summary(&self.stderr, self.code)))
    }
}

/// A name that says the archive has more volumes: `.part1.rar`, `.001`.
fn is_split_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    ext == "001" || (ext.eq_ignore_ascii_case("rar") && part_number(stem).is_some())
}

/// The error lines of 7-Zip's output, without the paths it repeats.
fn summary(stderr: &str, code: Option<i32>) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && *l != "ERRORS:")
        .map(|l| l.rsplit_once(" : ").map_or(l, |(_, why)| why))
        .take(3)
        .collect();
    match (lines.is_empty(), code) {
        (false, _) => lines.join("; "),
        (true, Some(code)) => format!("exit code {code}"),
        (true, None) => "it was stopped".into(),
    }
}

/// Run `command`, calling `on_line` for each line it prints. Stops it when
/// `cancel` fires or `on_line` fails.
async fn run(
    mut command: Command,
    cancel: &CancellationToken,
    mut on_line: impl FnMut(&str) -> Result<(), ArchiveError>,
) -> Result<Status, ArchiveError> {
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let read_stdout = async {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = (&mut reader)
                .take(MAX_LINE as u64)
                .read_until(b'\n', &mut line)
                .await?;
            if n == 0 {
                return Ok::<(), ArchiveError>(());
            }
            let text = String::from_utf8_lossy(&line);
            on_line(text.trim_end_matches(['\r', '\n']))?;
        }
    };
    let work = async {
        let (lines, stderr) = tokio::join!(read_stdout, read_bounded(stderr));
        lines?;
        let status = child.wait().await?;
        Ok::<_, ArchiveError>(Status {
            code: status.code(),
            stderr: stderr?,
        })
    };
    tokio::select! {
        result = work => result,
        () = cancel.cancelled() => Err(ArchiveError::Cancelled),
    }
    // Dropping the child on error or cancellation kills it (`kill_on_drop`).
}

/// Read all of `reader`, keeping the first [`MAX_STDERR`] bytes.
async fn read_bounded(reader: impl AsyncRead + Unpin) -> io::Result<String> {
    let mut kept = Vec::new();
    let mut reader = BufReader::new(reader);
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            break;
        }
        let room = MAX_STDERR.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..buf.len().min(room)]);
        let n = buf.len();
        reader.consume(n);
    }
    Ok(String::from_utf8_lossy(&kept).into_owned())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn archive_roles() {
        let first = |name: &str| assert_eq!(role(name), Some(Role::First), "{name}");
        let volume = |name: &str, of: &str| {
            assert_eq!(
                role(name),
                Some(Role::Volume { first: of.into() }),
                "{name}"
            )
        };
        let none = |name: &str| assert_eq!(role(name), None, "{name}");

        first("Show.S01.MULTi.1080p.zip");
        first("Show.S01.rar");
        first("Show.S01.7z");
        first("Show.S01.tar");
        first("Show.S01.part1.rar");
        first("Show.S01.part01.rar");
        first("Show.S01.Part001.RAR");
        first("Movie.2019.mkv.001");
        first("Movie.2019.7z.001");
        volume("Show.S01.part2.rar", "Show.S01.part1.rar");
        volume("Show.S01.part02.rar", "Show.S01.part01.rar");
        volume("Show.S01.Part010.RAR", "Show.S01.Part001.RAR");
        volume("Show.S01.r00", "Show.S01.rar");
        volume("Show.S01.R12", "Show.S01.RAR");
        volume("Show.S01.z01", "Show.S01.zip");
        volume("Movie.2019.mkv.002", "Movie.2019.mkv.001");
        none("Movie.2019.mkv");
        none("Movie.2019.iso");
        none("Movie.2019.tar.gz");
        none(".zip");
        none("noextension");
        none("Episode.002");
        // A "part" with too many digits, or none, is just a name.
        first("Show.part.rar");
        first("Show.part12345.rar");
    }

    #[test]
    fn archive_sets() {
        assert!(in_set("Show.part1.rar", "Show.part1.rar"));
        assert!(in_set("Show.part2.rar", "Show.part1.rar"));
        assert!(in_set("Show.r00", "Show.rar"));
        assert!(in_set("Movie.mkv.002", "Movie.mkv.001"));
        assert!(!in_set("Other.part2.rar", "Show.part1.rar"));
        assert!(!in_set("Show.part02.rar", "Show.part1.rar"));
        assert!(!in_set("Show.S01E01.mkv", "Show.zip"));
        assert!(!in_set("Show.rar", "Show.zip"));
    }

    #[test]
    fn samples() {
        assert!(is_sample("Show.S01/Sample/show.s01e01.mkv"));
        assert!(is_sample("show-sample.mkv"));
        assert!(is_sample("SAMPLE.mkv"));
        assert!(!is_sample("Show.S01/Show.S01E01.mkv"));
        assert!(!is_sample("Sampler.S01E01.mkv"));
    }

    #[test]
    fn unsafe_entries_are_refused() {
        let check = |path: &str| {
            check_entry(&Entry {
                path: path.into(),
                ..Default::default()
            })
        };
        check("Show.S01/Show.S01E01.mkv").unwrap();
        check("a..b/c...mkv").unwrap();
        for bad in [
            "/etc/passwd",
            "../escape.mkv",
            "Show/../../escape.mkv",
            "Show\\..\\..\\escape.mkv",
            "C:\\Windows\\x.mkv",
            "line\nbreak.mkv",
        ] {
            assert!(
                matches!(check(bad), Err(ArchiveError::Unsafe(_))),
                "{bad:?}"
            );
        }
        let link = Entry {
            path: "Show/link".into(),
            link: true,
            ..Default::default()
        };
        assert!(matches!(check_entry(&link), Err(ArchiveError::Unsafe(_))));
        let encrypted = Entry {
            path: "Show/E01.mkv".into(),
            encrypted: true,
            ..Default::default()
        };
        assert!(matches!(
            check_entry(&encrypted),
            Err(ArchiveError::Encrypted)
        ));
    }

    #[test]
    fn errors_are_classified() {
        let status = |code, stderr: &str| Status {
            code: Some(code),
            stderr: stderr.into(),
        };
        let path = |name: &str| PathBuf::from(format!("/dl/{name}"));
        assert!(status(0, "").check(&path("a.zip")).is_ok());
        assert!(
            status(1, "WARNING: headers error")
                .check(&path("a.zip"))
                .is_ok()
        );
        assert!(matches!(
            status(2, "ERROR: Wrong password : Show/E01.mkv").check(&path("a.zip")),
            Err(ArchiveError::Encrypted)
        ));
        assert!(matches!(
            status(
                2,
                "ERROR: a.7z : Cannot open encrypted archive. Wrong password?"
            )
            .check(&path("a.7z")),
            Err(ArchiveError::Encrypted)
        ));
        let truncated =
            "ERROR: a : Cannot open the file as [7z] archive\nERRORS:\nUnexpected end of archive";
        assert!(matches!(
            status(2, truncated).check(&path("a.7z.001")),
            Err(ArchiveError::MissingVolume)
        ));
        assert!(matches!(
            status(2, truncated).check(&path("a.part1.rar")),
            Err(ArchiveError::MissingVolume)
        ));
        assert!(matches!(
            status(0, "ERROR: Missing volume : a.r00").check(&path("a.rar")),
            Err(ArchiveError::MissingVolume)
        ));
        // A single archive that ends early is damaged, not incomplete.
        match status(2, truncated).check(&path("a.zip")) {
            Err(ArchiveError::Unreadable(why)) => {
                assert!(why.contains("Cannot open the file"), "{why}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// 7-Zip for the tests that need a real one: `DIREKT_TEST_7Z`, else one
    /// on the `PATH`. Those tests are skipped without it.
    pub(crate) fn real() -> Option<SevenZip> {
        match std::env::var_os("DIREKT_TEST_7Z") {
            Some(path) => Some(SevenZip::at(path)),
            None => SevenZip::find(),
        }
    }

    /// With a real 7-Zip (see [`real`]): archives it makes itself.
    #[tokio::test]
    async fn real_7zip_checks_archives() {
        let Some(z) = real() else {
            eprintln!("7-Zip not found; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = |p: &str| dir.path().join(p);
        let make = |args: &[&str], archive: &str, input: &str| {
            let ok = std::process::Command::new(z.program())
                .args(["a", "-bso0", "-bsp0"])
                .args(args)
                .arg("--")
                .arg(path(archive))
                .arg(path(input))
                .status()
                .unwrap()
                .success();
            assert!(ok, "7-Zip could not make {archive}");
        };
        let cancel = CancellationToken::new();
        std::fs::create_dir(path("Show.S01")).unwrap();
        let random: Vec<u8> = (0..300_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        std::fs::write(path("Show.S01/Show.S01E01.mkv"), &random).unwrap();

        make(&[], "plain.zip", "Show.S01");
        let entries = z.list(&path("plain.zip"), &cancel).await.unwrap();
        let file = entries.iter().find(|e| !e.is_dir).unwrap();
        assert_eq!(file.path, "Show.S01/Show.S01E01.mkv");
        assert_eq!(file.size, 300_000);
        assert!(entries.iter().any(|e| e.is_dir && e.path == "Show.S01"));

        make(&["-pSECRET"], "locked.zip", "Show.S01");
        assert!(matches!(
            z.list(&path("locked.zip"), &cancel).await,
            Err(ArchiveError::Encrypted)
        ));
        make(&["-pSECRET", "-mhe=on"], "locked.7z", "Show.S01");
        assert!(matches!(
            z.list(&path("locked.7z"), &cancel).await,
            Err(ArchiveError::Encrypted)
        ));

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", path("Show.S01/link")).unwrap();
            make(&["-snl"], "link.zip", "Show.S01");
            assert!(matches!(
                z.list(&path("link.zip"), &cancel).await,
                Err(ArchiveError::Unsafe(_))
            ));
            std::fs::remove_file(path("Show.S01/link")).unwrap();
        }

        // Volumes: complete, the set extracts; with one missing, it waits.
        make(&["-mx0", "-v100k"], "split.7z", "Show.S01");
        let out = path("out");
        std::fs::create_dir(&out).unwrap();
        z.extract(&path("split.7z.001"), &out, &cancel)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(out.join("Show.S01/Show.S01E01.mkv")).unwrap(),
            random
        );
        std::fs::remove_file(path("split.7z.002")).unwrap();
        assert!(matches!(
            z.list(&path("split.7z.001"), &cancel).await,
            Err(ArchiveError::MissingVolume)
        ));

        std::fs::write(path("not-an-archive.zip"), b"hello").unwrap();
        assert!(matches!(
            z.list(&path("not-an-archive.zip"), &cancel).await,
            Err(ArchiveError::Unreadable(_))
        ));
    }
}
