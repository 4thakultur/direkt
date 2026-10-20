//! A single transfer attempt: request, validate, and stream the body into the
//! partial file, resuming from an existing partial file when that is safe.
//!
//! Resume rules:
//! * Only when the provider allows it, a partial file exists, and a validator
//!   from the previous response is known: a strong ETag, or else Last-Modified.
//! * The request sends `Range` together with `If-Range`. If the remote file
//!   changed, the server answers 200 with the full body and we start over.
//! * A 206 is accepted only if its `Content-Range` starts exactly where the
//!   partial file ends.
//! * Without a usable validator the transfer restarts from zero. Appending
//!   bytes from a possibly different file would corrupt the download.

use super::{Inner, LiveProgress, StopReason, wait_for_stop};
use crate::domain::Download;
use crate::error::{ErrorKind, TransferError};
use crate::fsutil;
use crate::http_client::{
    check_destination, check_peer, classify_reqwest_error, classify_status, parse_retry_after,
};
use crate::providers::ResolvedResource;
use crate::store::TransferInfo;
use chrono::Utc;
use reqwest::StatusCode;
use reqwest::header::{self, HeaderMap};
use std::io::SeekFrom;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::sync::watch;
use url::Url;

const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
const DISK_CHECK_INTERVAL_BYTES: u64 = 64 * 1024 * 1024;
const WRITE_BUFFER: usize = 256 * 1024;
/// Wait after a provider link answered with an error page instead of a file.
const HTML_RESPONSE_RETRY: Duration = Duration::from_secs(5 * 60);

pub(super) enum AttemptOutcome {
    /// The partial file now holds the complete resource.
    Finished {
        bytes: u64,
        filename: String,
    },
    Stopped(StopReason),
}

pub(super) struct Attempt<'a> {
    pub inner: &'a Inner,
    pub download: &'a Download,
    pub resource: &'a ResolvedResource,
    pub part_path: &'a Path,
    pub live: &'a LiveProgress,
    pub stop: &'a mut watch::Receiver<Option<StopReason>>,
}

impl Attempt<'_> {
    pub async fn run(mut self) -> Result<AttemptOutcome, TransferError> {
        let mut may_restart = true;
        loop {
            match self.run_once().await? {
                Step::Done(outcome) => return Ok(outcome),
                Step::RestartFromZero(reason) if may_restart => {
                    tracing::info!(id = %self.download.id, reason, "discarding partial file and restarting");
                    remove_if_exists(self.part_path).await?;
                    may_restart = false;
                }
                Step::RestartFromZero(reason) => {
                    return Err(TransferError::retry(ErrorKind::Network, reason));
                }
            }
        }
    }

    async fn run_once(&mut self) -> Result<Step, TransferError> {
        let dl = self.download;
        let policy = &self.inner.policy;
        let existing = partial_len(self.part_path).await?;
        let resume = if existing > 0 && self.resource.resumable {
            resume_mode(dl, self.resource.stable_identity)
        } else {
            None
        };
        let offset = if resume.is_some() { existing } else { 0 };

        check_destination(&self.resource.url, policy).map_err(|_| {
            TransferError::fail(
                ErrorKind::InvalidRequest,
                "destination is a private, local, or reserved address, which is not allowed",
            )
        })?;
        let mut request = self.inner.client.get(self.resource.url.clone());
        match &resume {
            Some(Resume::IfRange(validator)) => {
                request = request
                    .header(header::RANGE, format!("bytes={offset}-"))
                    .header(header::IF_RANGE, validator);
            }
            Some(Resume::SameSize(_)) => {
                request = request.header(header::RANGE, format!("bytes={offset}-"));
            }
            None => {}
        }

        let response = tokio::select! {
            biased;
            reason = wait_for_stop(self.stop) => return Ok(Step::Done(AttemptOutcome::Stopped(reason))),
            r = request.send() => r.map_err(|e| classify_reqwest_error(&e))?,
        };
        check_peer(response.remote_addr(), policy)?;

        let status = response.status();
        let headers = response.headers().clone();
        let start = match status {
            StatusCode::PARTIAL_CONTENT if offset > 0 => match parse_content_range(&headers) {
                Some(range) if range.start == offset && resume_total_ok(&resume, range.total) => {
                    offset
                }
                _ => {
                    return Ok(Step::RestartFromZero(
                        "server returned an unexpected byte range",
                    ));
                }
            },
            StatusCode::OK => 0,
            StatusCode::RANGE_NOT_SATISFIABLE if offset > 0 => {
                // With a matching If-Range (or the same known size), a 416
                // whose total equals our partial length means the file was
                // already fully received.
                let total = unsatisfied_range_total(&headers);
                if total == Some(offset) && resume_total_ok(&resume, total) {
                    let filename = self.choose_filename(&headers, response.url());
                    return Ok(Step::Done(AttemptOutcome::Finished {
                        bytes: offset,
                        filename,
                    }));
                }
                return Ok(Step::RestartFromZero("server rejected the resume range"));
            }
            s if s.is_success() => {
                return Err(TransferError::fail(
                    ErrorKind::RemoteUnavailable,
                    format!("unexpected response from server (HTTP {})", s.as_u16()),
                ));
            }
            s => {
                let retry_after = headers
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| parse_retry_after(v, Utc::now()));
                return Err(classify_status(s, retry_after));
            }
        };

        if self.resource.reject_html && is_html(&headers) {
            return Err(TransferError::retry_after(
                ErrorKind::RemoteUnavailable,
                "server returned a web page instead of the file",
                Some(HTML_RESPONSE_RETRY),
            ));
        }

        let total = if status == StatusCode::PARTIAL_CONTENT {
            parse_content_range(&headers)
                .and_then(|r| r.total)
                .or_else(|| response.content_length().map(|len| start + len))
        } else {
            response.content_length()
        };
        let filename = self.choose_filename(&headers, response.url());
        let info = TransferInfo {
            total_bytes: total.map(saturating_i64),
            filename: Some(filename.clone()),
            etag: header_str(&headers, header::ETAG).filter(|e| !e.starts_with("W/")),
            last_modified: header_str(&headers, header::LAST_MODIFIED),
            downloaded_bytes: saturating_i64(start),
        };
        self.inner
            .store
            .update_transfer_info(dl.id, &info, Utc::now())
            .await?;
        self.live.set_total(total);
        self.live.set_downloaded(start);
        if start > 0 {
            tracing::info!(id = %dl.id, offset = start, "resuming partial download");
        }

        self.check_disk_space(total.map(|t| t.saturating_sub(start)))?;

        let file = open_partial(self.part_path, start)
            .await
            .map_err(|e| TransferError::from_io(&e, "opening the partial file"))?;
        let mut writer = BufWriter::with_capacity(WRITE_BUFFER, file);
        let mut written = start;
        let result = self
            .stream_body(response, &mut writer, &mut written, total)
            .await;

        // Persist whatever was received, whatever happened, so it can be resumed.
        let flushed = async {
            writer.flush().await?;
            writer.get_ref().sync_all().await
        }
        .await;
        let _ = self
            .inner
            .store
            .update_progress(dl.id, saturating_i64(written), None, None, Utc::now())
            .await;
        self.live.set_downloaded(written);

        let outcome = result?;
        flushed.map_err(|e| TransferError::from_io(&e, "writing the partial file"))?;
        if let AttemptOutcome::Stopped(_) = outcome {
            return Ok(Step::Done(outcome));
        }
        if let Some(total) = total
            && written < total
        {
            return Err(TransferError::retry(
                ErrorKind::Network,
                "connection closed before the transfer finished",
            ));
        }
        Ok(Step::Done(AttemptOutcome::Finished {
            bytes: written,
            filename,
        }))
    }

    async fn stream_body(
        &mut self,
        mut response: reqwest::Response,
        writer: &mut BufWriter<tokio::fs::File>,
        written: &mut u64,
        total: Option<u64>,
    ) -> Result<AttemptOutcome, TransferError> {
        let mut last_report = Instant::now();
        let mut bytes_at_last_report = *written;
        let mut since_disk_check = 0u64;
        loop {
            let chunk = tokio::select! {
                biased;
                reason = wait_for_stop(self.stop) => return Ok(AttemptOutcome::Stopped(reason)),
                c = response.chunk() => c.map_err(|e| classify_reqwest_error(&e))?,
            };
            let Some(chunk) = chunk else {
                // `filename` is filled in by the caller.
                return Ok(AttemptOutcome::Finished {
                    bytes: *written,
                    filename: String::new(),
                });
            };
            writer
                .write_all(&chunk)
                .await
                .map_err(|e| TransferError::from_io(&e, "writing the partial file"))?;
            let len = chunk.len() as u64;
            *written += len;
            since_disk_check += len;
            self.live.set_downloaded(*written);

            if let Some(total) = total
                && *written > total
            {
                return Err(TransferError::retry(
                    ErrorKind::Network,
                    "server sent more data than announced",
                ));
            }

            let elapsed = last_report.elapsed();
            if elapsed >= PROGRESS_INTERVAL {
                let rate = (*written - bytes_at_last_report) as f64 / elapsed.as_secs_f64();
                self.live.record(*written, rate);
                let _ = self
                    .inner
                    .store
                    .update_progress(
                        self.download.id,
                        saturating_i64(*written),
                        None,
                        None,
                        Utc::now(),
                    )
                    .await;
                last_report = Instant::now();
                bytes_at_last_report = *written;
            }
            if since_disk_check >= DISK_CHECK_INTERVAL_BYTES {
                since_disk_check = 0;
                self.check_disk_space(total.map(|t| t - *written))?;
            }
        }
    }

    /// Ensure the incomplete directory has room for `remaining` bytes plus the
    /// configured reserve.
    fn check_disk_space(&self, remaining: Option<u64>) -> Result<(), TransferError> {
        let dir = &self.inner.settings.incomplete_dir;
        let available = fsutil::available_space(dir)
            .map_err(|e| TransferError::from_io(&e, "checking free disk space"))?;
        let needed = remaining
            .unwrap_or(0)
            .saturating_add(self.inner.settings.min_free_bytes);
        if available < needed {
            return Err(TransferError::block(
                ErrorKind::InsufficientDiskSpace,
                format!(
                    "not enough free disk space: {} MiB needed, {} MiB available",
                    needed.div_ceil(1024 * 1024),
                    available / (1024 * 1024)
                ),
            ));
        }
        Ok(())
    }

    fn choose_filename(&self, headers: &HeaderMap, final_url: &Url) -> String {
        headers
            .get(header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .and_then(content_disposition_filename)
            .and_then(|n| fsutil::sanitize_filename(&n))
            .or_else(|| {
                self.resource
                    .filename_hint
                    .as_deref()
                    .and_then(fsutil::sanitize_filename)
            })
            .or_else(|| filename_from_url(&self.resource.url))
            .or_else(|| filename_from_url(final_url))
            .or_else(|| self.download.filename.clone())
            .unwrap_or_else(|| "download".to_owned())
    }
}

enum Step {
    Done(AttemptOutcome),
    RestartFromZero(&'static str),
}

fn saturating_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// How an existing partial file may be resumed.
#[derive(Debug, PartialEq, Eq)]
enum Resume {
    /// `Range` + `If-Range`: the server only honors the range if the file
    /// still matches this validator.
    IfRange(String),
    /// The provider guarantees the file's identity; the range is accepted only
    /// if the server reports exactly this total size.
    SameSize(u64),
}

fn resume_mode(dl: &Download, stable_identity: bool) -> Option<Resume> {
    if stable_identity {
        // Validators may differ between the servers a provider's links point
        // to, so rely on the provider's identity guarantee plus the size.
        return dl
            .total_bytes
            .and_then(|t| u64::try_from(t).ok())
            .map(Resume::SameSize);
    }
    if_range_validator(dl).map(Resume::IfRange)
}

fn resume_total_ok(resume: &Option<Resume>, total: Option<u64>) -> bool {
    match resume {
        Some(Resume::SameSize(expected)) => total == Some(*expected),
        _ => true,
    }
}

fn is_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"))
}

/// The `If-Range` value to resume with: a strong ETag, or else Last-Modified.
fn if_range_validator(dl: &Download) -> Option<String> {
    dl.resume_etag
        .clone()
        .filter(|e| !e.starts_with("W/"))
        .or_else(|| dl.resume_last_modified.clone())
}

/// Size of an existing partial file. Refuses anything but a regular file, so a
/// planted symlink or directory is never written through.
async fn partial_len(path: &Path) -> Result<u64, TransferError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(meta) if meta.file_type().is_file() => Ok(meta.len()),
        Ok(_) => Err(TransferError::block(
            ErrorKind::Filesystem,
            "partial download path exists but is not a regular file",
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(TransferError::from_io(&e, "inspecting the partial file")),
    }
}

async fn open_partial(path: &Path, start: u64) -> std::io::Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true);
    #[cfg(target_os = "linux")]
    options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    let mut file = options.open(path).await?;
    file.set_len(start).await?;
    file.seek(SeekFrom::Start(start)).await?;
    Ok(file)
}

pub(super) async fn remove_if_exists(path: &Path) -> Result<(), TransferError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(TransferError::from_io(&e, "removing the partial file")),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ContentRange {
    start: u64,
    total: Option<u64>,
}

/// Parse `Content-Range: bytes <start>-<end>/<total|*>`.
fn parse_content_range(headers: &HeaderMap) -> Option<ContentRange> {
    let value = headers.get(header::CONTENT_RANGE)?.to_str().ok()?;
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    if end < start {
        return None;
    }
    let total = match total.trim() {
        "*" => None,
        t => {
            let t: u64 = t.parse().ok()?;
            if end >= t {
                return None;
            }
            Some(t)
        }
    };
    Some(ContentRange { start, total })
}

/// Parse the total from a 416 response's `Content-Range: bytes */<total>`.
fn unsatisfied_range_total(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(header::CONTENT_RANGE)?.to_str().ok()?;
    value.trim().strip_prefix("bytes */")?.trim().parse().ok()
}

/// Extract the filename from a `Content-Disposition` header, preferring the
/// RFC 6266 `filename*` form. The result is untrusted and must be sanitized.
fn content_disposition_filename(value: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for param in split_params(value).into_iter().skip(1) {
        let Some((key, val)) = param.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                // charset'language'percent-encoded
                let mut parts = val.trim().splitn(3, '\'');
                let (Some(charset), Some(_lang), Some(encoded)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                if charset.eq_ignore_ascii_case("utf-8") {
                    extended = percent_encoding::percent_decode_str(encoded)
                        .decode_utf8()
                        .ok()
                        .map(|s| s.into_owned());
                }
            }
            "filename" => plain = Some(unquote(val.trim())),
            _ => {}
        }
    }
    extended.or(plain).filter(|s| !s.is_empty())
}

/// Split header parameters on `;`, respecting quoted strings.
fn split_params(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_quotes = false;
    let mut escaped = false;
    let mut start = 0;
    for (i, c) in value.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => {
                parts.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

fn unquote(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_owned();
    };
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Last non-empty path segment of a URL, percent-decoded and sanitized.
fn filename_from_url(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.rev().find(|s| !s.is_empty())?;
    let decoded = percent_encoding::percent_decode_str(segment).decode_utf8_lossy();
    fsutil::sanitize_filename(&decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn headers(name: header::HeaderName, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn content_range_parsing() {
        let p = |v: &str| parse_content_range(&headers(header::CONTENT_RANGE, v));
        assert_eq!(
            p("bytes 100-199/1000"),
            Some(ContentRange {
                start: 100,
                total: Some(1000)
            })
        );
        assert_eq!(
            p("bytes 100-199/*"),
            Some(ContentRange {
                start: 100,
                total: None
            })
        );
        assert_eq!(p("bytes 200-100/1000"), None);
        assert_eq!(p("bytes 100-1000/1000"), None);
        assert_eq!(p("bytes */1000"), None);
        assert_eq!(p("items 1-2/3"), None);
        assert_eq!(
            unsatisfied_range_total(&headers(header::CONTENT_RANGE, "bytes */1000")),
            Some(1000)
        );
    }

    #[test]
    fn content_disposition_parsing() {
        let cd = content_disposition_filename;
        assert_eq!(
            cd("attachment; filename=\"movie.mkv\"").as_deref(),
            Some("movie.mkv")
        );
        assert_eq!(
            cd("attachment; filename=plain.zip").as_deref(),
            Some("plain.zip")
        );
        assert_eq!(
            cd("attachment; filename=\"fallback.txt\"; filename*=UTF-8''%C3%A9t%C3%A9.txt")
                .as_deref(),
            Some("été.txt")
        );
        assert_eq!(
            cd("attachment; filename=\"a;b \\\"q\\\".txt\"").as_deref(),
            Some("a;b \"q\".txt")
        );
        assert_eq!(cd("inline").as_deref(), None);
        assert_eq!(cd("attachment; filename=\"\"").as_deref(), None);
        // Path components are only removed by sanitization afterwards.
        assert_eq!(
            cd("attachment; filename=\"../../etc/passwd\"")
                .and_then(|n| fsutil::sanitize_filename(&n)),
            Some("_.._etc_passwd".to_owned())
        );
    }

    #[test]
    fn url_filenames() {
        let f = |s: &str| filename_from_url(&Url::parse(s).unwrap());
        assert_eq!(
            f("https://x.org/a/b/My%20File.mkv?token=1").as_deref(),
            Some("My File.mkv")
        );
        assert_eq!(f("https://x.org/dir/").as_deref(), Some("dir"));
        assert_eq!(f("https://x.org/").as_deref(), None);
        assert_eq!(f("https://x.org/%2e%2e").as_deref(), None);
        assert_eq!(
            f("https://x.org/a%2F..%2F..%2Fetc").as_deref(),
            Some("a_.._.._etc")
        );
    }

    #[test]
    fn validator_preference() {
        let mut dl = Download {
            id: uuid::Uuid::nil(),
            url: String::new(),
            display_url: String::new(),
            provider: "http".into(),
            state: crate::domain::TransferState::Downloading,
            post_state: crate::domain::PostState::None,
            filename: None,
            total_bytes: None,
            downloaded_bytes: 0,
            retry_count: 0,
            next_attempt_at: None,
            error: None,
            post_error: None,
            file_path: None,
            resume_etag: Some("W/\"weak\"".into()),
            resume_last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
            download_password: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: None,
        };
        assert_eq!(
            if_range_validator(&dl).as_deref(),
            Some("Wed, 21 Oct 2015 07:28:00 GMT")
        );
        dl.resume_etag = Some("\"strong\"".into());
        assert_eq!(if_range_validator(&dl).as_deref(), Some("\"strong\""));
        dl.resume_etag = None;
        dl.resume_last_modified = None;
        assert_eq!(if_range_validator(&dl), None);

        // Stable provider identity: resume by size, never by validator.
        dl.resume_etag = Some("\"strong\"".into());
        assert_eq!(resume_mode(&dl, true), None, "size unknown");
        dl.total_bytes = Some(1000);
        assert_eq!(resume_mode(&dl, true), Some(Resume::SameSize(1000)));
        assert_eq!(
            resume_mode(&dl, false),
            Some(Resume::IfRange("\"strong\"".into()))
        );
        assert!(resume_total_ok(&Some(Resume::SameSize(1000)), Some(1000)));
        assert!(!resume_total_ok(&Some(Resume::SameSize(1000)), Some(999)));
        assert!(!resume_total_ok(&Some(Resume::SameSize(1000)), None));
    }
}
