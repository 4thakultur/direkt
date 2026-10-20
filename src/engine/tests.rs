//! Engine tests against a local HTTP server. No external network access.

use super::*;
use crate::service::DownloadService;
use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use std::collections::HashMap as StdHashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

// ---------------------------------------------------------------------------
// Test server
// ---------------------------------------------------------------------------

struct ServerState {
    content: Vec<u8>,
    etag: Mutex<String>,
    /// Delay between 16 KiB chunks, for pause/cancel tests.
    chunk_delay: Mutex<Duration>,
    /// Number of upcoming requests to answer with 503.
    fail_next: AtomicUsize,
    /// Cut the next response off halfway through the body.
    truncate_next: AtomicBool,
    hits: AtomicUsize,
    range_headers: Mutex<Vec<Option<String>>>,
    if_range_headers: Mutex<Vec<Option<String>>>,
    /// Report a wrong total size in the next 206 response.
    wrong_total_next: AtomicBool,
    /// Base URL of this server, for links handed out by the mock API.
    base: Mutex<String>,
    /// Scripted responses for the mock 1fichier API, consumed in order.
    /// When empty, the API returns a link to `/plainrange/movie.bin`.
    api_script: Mutex<std::collections::VecDeque<(u16, String)>>,
    api_calls: Mutex<Vec<ApiCall>>,
}

#[derive(Debug, Clone)]
struct ApiCall {
    authorization: Option<String>,
    body: serde_json::Value,
    at: Instant,
}

struct TestServer {
    addr: SocketAddr,
    state: Arc<ServerState>,
}

impl TestServer {
    async fn start(ip: [u8; 4], content: Vec<u8>) -> Self {
        let state = Arc::new(ServerState {
            content,
            etag: Mutex::new("\"v1\"".into()),
            chunk_delay: Mutex::new(Duration::ZERO),
            fail_next: AtomicUsize::new(0),
            truncate_next: AtomicBool::new(false),
            hits: AtomicUsize::new(0),
            range_headers: Mutex::new(Vec::new()),
            if_range_headers: Mutex::new(Vec::new()),
            wrong_total_next: AtomicBool::new(false),
            base: Mutex::new(String::new()),
            api_script: Mutex::new(Default::default()),
            api_calls: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/files/{name}", get(serve_file))
            .route("/norange/{name}", get(serve_no_range))
            .route("/disposition", get(serve_disposition))
            .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
            .route("/redirect", get(redirect))
            .route("/plainrange/{name}", get(serve_plain_range))
            .route("/html", get(serve_html))
            .route(
                "/v1/download/get_token.cgi",
                axum::routing::post(mock_get_token),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((ip, 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        *state.base.lock().unwrap() = format!("http://{addr}");
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { addr, state }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn hits(&self) -> usize {
        self.state.hits.load(Ordering::SeqCst)
    }

    fn set_delay(&self, delay: Duration) {
        *self.state.chunk_delay.lock().unwrap() = delay;
    }

    fn ranges(&self) -> Vec<Option<String>> {
        self.state.range_headers.lock().unwrap().clone()
    }

    fn if_ranges(&self) -> Vec<Option<String>> {
        self.state.if_range_headers.lock().unwrap().clone()
    }

    fn api_calls(&self) -> Vec<ApiCall> {
        self.state.api_calls.lock().unwrap().clone()
    }

    fn script_api(&self, status: u16, body: serde_json::Value) {
        self.state
            .api_script
            .lock()
            .unwrap()
            .push_back((status, body.to_string()));
    }

    fn api_base(&self) -> url::Url {
        url::Url::parse(&self.url("/v1/")).unwrap()
    }
}

fn content(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

fn stream(data: Vec<u8>, delay: Duration, cut_at: Option<usize>) -> Body {
    let (mut writer, reader) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let end = cut_at.unwrap_or(data.len());
        for chunk in data[..end].chunks(16 * 1024) {
            if writer.write_all(chunk).await.is_err() {
                return;
            }
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
    });
    Body::from_stream(ReaderStream::new(reader))
}

fn record(state: &ServerState, headers: &HeaderMap) -> Option<Response> {
    state.hits.fetch_add(1, Ordering::SeqCst);
    let header_value = |name| {
        headers
            .get(name)
            .map(|v: &header::HeaderValue| v.to_str().unwrap().to_owned())
    };
    state
        .range_headers
        .lock()
        .unwrap()
        .push(header_value(header::RANGE));
    state
        .if_range_headers
        .lock()
        .unwrap()
        .push(header_value(header::IF_RANGE));
    let failing = state
        .fail_next
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok();
    failing.then(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "0")],
        )
            .into_response()
    })
}

async fn serve_file(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    if let Some(resp) = record(&state, &headers) {
        return resp;
    }
    let etag = state.etag.lock().unwrap().clone();
    let delay = *state.chunk_delay.lock().unwrap();
    let len = state.content.len();
    let truncate = state.truncate_next.swap(false, Ordering::SeqCst);

    let if_range_ok = headers
        .get(header::IF_RANGE)
        .is_none_or(|v| v.to_str().unwrap() == etag);
    let range_start = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.strip_suffix('-'))
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|_| if_range_ok);

    match range_start {
        Some(start) if start >= len => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{len}"))],
        )
            .into_response(),
        Some(start) => {
            let body = state.content[start..].to_vec();
            let cut = truncate.then_some(body.len() / 2);
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::ETAG, &etag)
                .header(header::CONTENT_LENGTH, body.len())
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{}/{len}", len - 1),
                )
                .body(stream(body, delay, cut))
                .unwrap()
        }
        None => {
            let cut = truncate.then_some(len / 2);
            Response::builder()
                .header(header::ETAG, &etag)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_LENGTH, len)
                .body(stream(state.content.clone(), delay, cut))
                .unwrap()
        }
    }
}

/// Range support without ETag/Last-Modified, like a token-based CDN link.
async fn serve_plain_range(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    if let Some(resp) = record(&state, &headers) {
        return resp;
    }
    let delay = *state.chunk_delay.lock().unwrap();
    let len = state.content.len();
    let range_start = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.strip_suffix('-'))
        .and_then(|v| v.parse::<usize>().ok());
    let builder = Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"Movie.2024.mkv\"",
        );
    match range_start {
        Some(start) if start < len => {
            let reported = if state.wrong_total_next.swap(false, Ordering::SeqCst) {
                len + 1
            } else {
                len
            };
            let body = state.content[start..].to_vec();
            builder
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_LENGTH, body.len())
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{}/{reported}", len - 1),
                )
                .body(stream(body, delay, None))
                .unwrap()
        }
        _ => builder
            .header(header::CONTENT_LENGTH, len)
            .body(stream(state.content.clone(), delay, None))
            .unwrap(),
    }
}

async fn serve_html(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    record(&state, &headers);
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        "<html><body>Your requests are too fast</body></html>",
    )
        .into_response()
}

/// Mock of `POST /v1/download/get_token.cgi`.
async fn mock_get_token(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    state.api_calls.lock().unwrap().push(ApiCall {
        authorization: headers
            .get(header::AUTHORIZATION)
            .map(|v| v.to_str().unwrap().to_owned()),
        body: serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
        at: Instant::now(),
    });
    let scripted = state.api_script.lock().unwrap().pop_front();
    let (status, body) = scripted.unwrap_or_else(|| {
        let base = state.base.lock().unwrap().clone();
        (
            200,
            serde_json::json!({"status": "OK", "url": format!("{base}/plainrange/movie.bin")})
                .to_string(),
        )
    });
    (
        StatusCode::from_u16(status).unwrap(),
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

async fn serve_no_range(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    if let Some(resp) = record(&state, &headers) {
        return resp;
    }
    let delay = *state.chunk_delay.lock().unwrap();
    Response::builder()
        .header(header::CONTENT_LENGTH, state.content.len())
        .body(stream(state.content.clone(), delay, None))
        .unwrap()
}

async fn serve_disposition(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    record(&state, &headers);
    Response::builder()
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"../../etc/evil.sh\"",
        )
        .body(Body::from(state.content.clone()))
        .unwrap()
}

async fn redirect(Query(q): Query<StdHashMap<String, String>>) -> Response {
    (StatusCode::FOUND, [(header::LOCATION, q["to"].clone())]).into_response()
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    providers: Providers,
    store: Store,
    service: DownloadService,
    engine: Engine,
    dispatcher: Option<JoinHandle<()>>,
    download_dir: PathBuf,
    incomplete_dir: PathBuf,
    _dirs: tempfile::TempDir,
}

/// 1fichier (API at `api_base`, which defaults to an unreachable address)
/// followed by plain HTTP, like production.
fn standard_test_providers(
    client: reqwest::Client,
    onefichier: Option<(url::Url, Option<crate::config::Secret>, Duration)>,
) -> Providers {
    let (api_base, key, interval) = onefichier.unwrap_or_else(|| {
        (
            url::Url::parse("http://127.0.0.1:9/v1/").unwrap(),
            None,
            Duration::ZERO,
        )
    });
    Providers::new(vec![
        Arc::new(crate::providers::OneFichierProvider::with_endpoint(
            client,
            crate::settings::SecretCell::new(key),
            api_base,
            interval,
        )),
        Arc::new(crate::providers::HttpProvider),
    ])
}

fn loopback_policy() -> NetworkPolicy {
    NetworkPolicy::new(vec!["127.0.0.1/32".parse().unwrap()])
}

impl Harness {
    async fn new(policy: NetworkPolicy, tweak: impl FnOnce(&mut EngineSettings)) -> Self {
        Self::with_providers(policy, tweak, |client| {
            standard_test_providers(client, None)
        })
        .await
    }

    async fn with_providers(
        policy: NetworkPolicy,
        tweak: impl FnOnce(&mut EngineSettings),
        make_providers: impl FnOnce(reqwest::Client) -> Providers,
    ) -> Self {
        let dirs = tempfile::tempdir().unwrap();
        let download_dir = dirs.path().join("downloads");
        let incomplete_dir = dirs.path().join("incomplete");
        std::fs::create_dir_all(&download_dir).unwrap();
        std::fs::create_dir_all(&incomplete_dir).unwrap();
        let mut settings = EngineSettings {
            download_dir: download_dir.clone(),
            incomplete_dir: incomplete_dir.clone(),
            max_concurrent: 2,
            max_retries: 3,
            retry_base_delay: Duration::from_millis(50),
            retry_max_delay: Duration::from_millis(200),
            min_free_bytes: 0,
        };
        tweak(&mut settings);
        let store = Store::open_in_memory().await.unwrap();
        let client = crate::http_client::build_client(policy.clone()).unwrap();
        let providers = make_providers(client.clone());
        let engine = Engine::new(
            store.clone(),
            providers.clone(),
            client,
            policy.clone(),
            settings,
        );
        let dispatcher = Some(engine.start());
        let service =
            DownloadService::new(store.clone(), engine.clone(), providers.clone(), policy);
        Self {
            providers,
            store,
            service,
            engine,
            dispatcher,
            download_dir,
            incomplete_dir,
            _dirs: dirs,
        }
    }

    /// Insert a download directly, bypassing submission-time URL validation,
    /// to exercise the engine's own connection-time checks.
    async fn insert_raw(&self, url: &str) -> Download {
        let d = self
            .store
            .insert_download(
                crate::store::NewDownload {
                    url: url.into(),
                    display_url: url.into(),
                    provider: "http".into(),
                    password: None,
                },
                Utc::now(),
            )
            .await
            .unwrap();
        self.engine.wake();
        d
    }

    async fn wait_for(&self, id: Uuid, pred: impl Fn(&Download) -> bool) -> Download {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let d = self.service.get(id).await.unwrap();
            if pred(&d) {
                return d;
            }
            assert!(
                Instant::now() < deadline,
                "timed out; download is {:?} ({:?})",
                d.state,
                d.error
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_state(&self, id: Uuid, state: TransferState) -> Download {
        self.wait_for(id, |d| d.state == state).await
    }

    async fn wait_progress(&self, id: Uuid, at_least: u64) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while self
            .engine
            .live(id)
            .is_none_or(|l| l.downloaded_bytes < at_least)
        {
            assert!(Instant::now() < deadline, "no progress");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn part_path(&self, id: Uuid) -> PathBuf {
        self.incomplete_dir.join(format!("{id}.part"))
    }

    async fn shutdown(&mut self) {
        if let Some(handle) = self.dispatcher.take() {
            self.engine.shutdown(handle).await;
        }
    }
}

fn file_contents(d: &Download) -> Vec<u8> {
    std::fs::read(d.file_path.as_ref().expect("file_path set")).unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn downloads_a_file() {
    let server = TestServer::start([127, 0, 0, 1], content(300_000)).await;
    let h = Harness::new(loopback_policy(), |_| {}).await;
    let d = h
        .service
        .submit(&server.url("/files/My%20Movie.mkv?token=abc"), None)
        .await
        .unwrap();
    assert_eq!(d.display_url, server.url("/files/My%20Movie.mkv?…"));

    let d = h.wait_state(d.id, TransferState::Completed).await;
    assert_eq!(d.post_state, crate::domain::PostState::Pending);
    assert_eq!(d.filename.as_deref(), Some("My Movie.mkv"));
    assert_eq!(d.total_bytes, Some(300_000));
    assert_eq!(d.downloaded_bytes, 300_000);
    assert_eq!(
        PathBuf::from(d.file_path.as_ref().unwrap()),
        h.download_dir.join("My Movie.mkv")
    );
    assert_eq!(file_contents(&d), server.state.content);
    assert!(!h.part_path(d.id).exists());
    assert!(h.engine.live(d.id).is_none());
}

#[tokio::test]
async fn hostile_content_disposition_is_sanitized() {
    let server = TestServer::start([127, 0, 0, 1], content(1000)).await;
    let h = Harness::new(loopback_policy(), |_| {}).await;
    let d = h
        .service
        .submit(&server.url("/disposition"), None)
        .await
        .unwrap();
    let d = h.wait_state(d.id, TransferState::Completed).await;
    let path = PathBuf::from(d.file_path.unwrap());
    assert_eq!(path.parent().unwrap(), h.download_dir);
    assert_eq!(path.file_name().unwrap(), "_.._etc_evil.sh");
}

#[tokio::test]
async fn existing_files_are_never_overwritten() {
    let server = TestServer::start([127, 0, 0, 1], content(1000)).await;
    let h = Harness::new(loopback_policy(), |_| {}).await;
    std::fs::write(h.download_dir.join("file.bin"), b"precious").unwrap();
    let d = h
        .service
        .submit(&server.url("/files/file.bin"), None)
        .await
        .unwrap();
    let d = h.wait_state(d.id, TransferState::Completed).await;
    assert_eq!(
        PathBuf::from(d.file_path.as_ref().unwrap()),
        h.download_dir.join("file (1).bin")
    );
    assert_eq!(
        std::fs::read(h.download_dir.join("file.bin")).unwrap(),
        b"precious"
    );
}

#[tokio::test]
async fn transient_errors_are_retried() {
    let server = TestServer::start([127, 0, 0, 1], content(5000)).await;
    server.state.fail_next.store(2, Ordering::SeqCst);
    let h = Harness::new(loopback_policy(), |_| {}).await;
    let d = h
        .service
        .submit(&server.url("/files/a.bin"), None)
        .await
        .unwrap();
    let d = h.wait_state(d.id, TransferState::Completed).await;
    assert_eq!(d.retry_count, 2);
    assert_eq!(d.error, None, "error clears on success");
    assert_eq!(server.hits(), 3);
    assert_eq!(file_contents(&d), server.state.content);

    let kinds: Vec<_> = h
        .service
        .events(d.id)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "submitted",
            "downloading",
            "retrying",
            "downloading",
            "retrying",
            "downloading",
            "completed"
        ]
    );
}

#[tokio::test]
async fn retries_are_bounded() {
    let server = TestServer::start([127, 0, 0, 1], content(100)).await;
    server.state.fail_next.store(usize::MAX, Ordering::SeqCst);
    let h = Harness::new(loopback_policy(), |s| s.max_retries = 2).await;
    let d = h
        .service
        .submit(&server.url("/files/a.bin"), None)
        .await
        .unwrap();
    let d = h.wait_state(d.id, TransferState::Failed).await;
    assert_eq!(server.hits(), 3, "initial attempt + 2 retries");
    let err = d.error.unwrap();
    assert_eq!(err.kind, ErrorKind::Network);
    assert!(
        err.message.contains("gave up after 2 retries"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn permanent_errors_fail_immediately() {
    let server = TestServer::start([127, 0, 0, 1], vec![]).await;
    let h = Harness::new(loopback_policy(), |_| {}).await;
    let d = h
        .service
        .submit(&server.url("/missing"), None)
        .await
        .unwrap();
    let d = h.wait_state(d.id, TransferState::Failed).await;
    assert_eq!(d.retry_count, 0);
    assert_eq!(d.error.unwrap().kind, ErrorKind::RemoteUnavailable);
}

#[tokio::test]
async fn truncated_transfer_resumes_with_range() {
    let server = TestServer::start([127, 0, 0, 1], content(400_000)).await;
    server.state.truncate_next.store(true, Ordering::SeqCst);
    let h = Harness::new(loopback_policy(), |_| {}).await;
    let d = h
        .service
        .submit(&server.url("/files/a.bin"), None)
        .await
        .unwrap();
    let d = h.wait_state(d.id, TransferState::Completed).await;
    assert_eq!(d.retry_count, 1);
    assert_eq!(file_contents(&d), server.state.content);
    let ranges = server.ranges();
    assert_eq!(ranges.len(), 2);
    assert_eq!(ranges[0], None);
    let resumed_from: usize = ranges[1]
        .as_deref()
        .expect("second request must resume")
        .trim_start_matches("bytes=")
        .trim_end_matches('-')
        .parse()
        .unwrap();
    assert!(resumed_from > 0 && resumed_from < 400_000);
}

#[tokio::test]
async fn pause_keeps_partial_and_resume_continues() {
    let server = TestServer::start([127, 0, 0, 1], content(2_000_000)).await;
    server.set_delay(Duration::from_millis(5));
    let h = Harness::new(loopback_policy(), |_| {}).await;
    let d = h
        .service
        .submit(&server.url("/files/big.bin"), None)
        .await
        .unwrap();
    h.wait_progress(d.id, 100_000).await;

    let paused = h.service.pause(d.id).await.unwrap();
    assert_eq!(paused.state, TransferState::Paused);
    let partial = std::fs::metadata(h.part_path(d.id)).unwrap().len();
    assert!(partial > 0);
    assert_eq!(
        paused.downloaded_bytes as u64, partial,
        "progress persisted on pause"
    );
    // Pausing again is a no-op.
    assert_eq!(
        h.service.pause(d.id).await.unwrap().state,
        TransferState::Paused
    );

    server.set_delay(Duration::ZERO);
    h.service.resume(d.id).await.unwrap();
    let d = h.wait_state(d.id, TransferState::Completed).await;
    assert_eq!(file_contents(&d), server.state.content);
    assert_eq!(
        server.ranges().last().unwrap().as_deref(),
        Some(format!("bytes={partial}-").as_str())
    );
}

#[tokio::test]
async fn changed_remote_file_restarts_from_zero() {
    let server = TestServer::start([127, 0, 0, 1], content(2_000_000)).await;
    server.set_delay(Duration::from_millis(5));
    let h = Harness::new(loopback_policy(), |_| {}).await;
    let d = h
        .service
        .submit(&server.url("/files/big.bin"), None)
        .await
        .unwrap();
    h.wait_progress(d.id, 100_000).await;
    h.service.pause(d.id).await.unwrap();

    // The remote file changes: If-Range no longer matches, server sends 200.
    *server.state.etag.lock().unwrap() = "\"v2\"".into();
    server.set_delay(Duration::ZERO);
    h.service.resume(d.id).await.unwrap();
    let d = h.wait_state(d.id, TransferState::Completed).await;
    assert_eq!(file_contents(&d), server.state.content);
}

#[tokio::test]
async fn server_without_validators_is_not_resumed() {
    let server = TestServer::start([127, 0, 0, 1], content(2_000_000)).await;
    server.set_delay(Duration::from_millis(5));
    let h = Harness::new(loopback_policy(), |_| {}).await;
    let d = h
        .service
        .submit(&server.url("/norange/big.bin"), None)
        .await
        .unwrap();
    h.wait_progress(d.id, 100_000).await;
    h.service.pause(d.id).await.unwrap();
    server.set_delay(Duration::ZERO);
    h.service.resume(d.id).await.unwrap();
    let d = h.wait_state(d.id, TransferState::Completed).await;
    assert_eq!(file_contents(&d), server.state.content);
    assert!(
        server.ranges().iter().all(Option::is_none),
        "no Range without a validator"
    );
}

#[tokio::test]
async fn cancel_removes_partial_data() {
    let server = TestServer::start([127, 0, 0, 1], content(2_000_000)).await;
    server.set_delay(Duration::from_millis(5));
    let h = Harness::new(loopback_policy(), |_| {}).await;
    let d = h
        .service
        .submit(&server.url("/files/big.bin"), None)
        .await
        .unwrap();
    h.wait_progress(d.id, 50_000).await;
    let cancelled = h.service.cancel(d.id).await.unwrap();
    assert_eq!(cancelled.state, TransferState::Cancelled);
    assert!(!h.part_path(d.id).exists());

    // Completed downloads cannot be cancelled.
    server.set_delay(Duration::ZERO);
    let d2 = h
        .service
        .submit(&server.url("/files/small.bin"), None)
        .await
        .unwrap();
    h.wait_state(d2.id, TransferState::Completed).await;
    let err = h.service.cancel(d2.id).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict);
}

#[tokio::test]
async fn pause_and_cancel_queued_jobs() {
    let server = TestServer::start([127, 0, 0, 1], content(2_000_000)).await;
    server.set_delay(Duration::from_millis(20));
    let h = Harness::new(loopback_policy(), |s| s.max_concurrent = 1).await;
    let running = h
        .service
        .submit(&server.url("/files/a.bin"), None)
        .await
        .unwrap();
    let queued = h
        .service
        .submit(&server.url("/files/b.bin"), None)
        .await
        .unwrap();
    h.wait_progress(running.id, 1).await;

    assert_eq!(
        h.service.get(queued.id).await.unwrap().state,
        TransferState::Queued
    );
    assert_eq!(
        h.service.pause(queued.id).await.unwrap().state,
        TransferState::Paused
    );
    assert_eq!(
        h.service.cancel(queued.id).await.unwrap().state,
        TransferState::Cancelled
    );
    h.service.cancel(running.id).await.unwrap();
}

#[tokio::test]
async fn concurrency_is_bounded() {
    let server = TestServer::start([127, 0, 0, 1], content(1_000_000)).await;
    server.set_delay(Duration::from_millis(10));
    let h = Harness::new(loopback_policy(), |s| s.max_concurrent = 2).await;
    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(
            h.service
                .submit(&server.url(&format!("/files/{i}.bin")), None)
                .await
                .unwrap()
                .id,
        );
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut max_seen = 0;
    loop {
        let downloading = h
            .store
            .list_downloads(&[TransferState::Downloading], 10, 0)
            .await
            .unwrap()
            .len();
        max_seen = max_seen.max(downloading);
        let completed = h
            .store
            .list_downloads(&[TransferState::Completed], 10, 0)
            .await
            .unwrap()
            .len();
        if completed == 4 {
            break;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(max_seen, 2);
}

#[tokio::test]
async fn shutdown_requeues_running_transfers() {
    let server = TestServer::start([127, 0, 0, 1], content(2_000_000)).await;
    server.set_delay(Duration::from_millis(5));
    let mut h = Harness::new(loopback_policy(), |_| {}).await;
    let d = h
        .service
        .submit(&server.url("/files/big.bin"), None)
        .await
        .unwrap();
    h.wait_progress(d.id, 50_000).await;
    h.shutdown().await;
    let d = h.service.get(d.id).await.unwrap();
    assert_eq!(d.state, TransferState::Queued);
    assert!(d.downloaded_bytes > 0);
    assert_eq!(
        std::fs::metadata(h.part_path(d.id)).unwrap().len(),
        d.downloaded_bytes as u64
    );
}

#[tokio::test]
async fn insufficient_disk_space_blocks() {
    let server = TestServer::start([127, 0, 0, 1], content(1000)).await;
    let h = Harness::new(loopback_policy(), |s| s.min_free_bytes = u64::MAX / 2).await;
    let d = h
        .service
        .submit(&server.url("/files/a.bin"), None)
        .await
        .unwrap();
    let d = h.wait_state(d.id, TransferState::Blocked).await;
    assert_eq!(d.error.unwrap().kind, ErrorKind::InsufficientDiskSpace);

    // Retrying is the user's way out once space is freed.
    assert_eq!(
        h.service.retry(d.id, None).await.unwrap().state,
        TransferState::Queued
    );
}

#[tokio::test]
async fn completion_survives_crash_between_move_and_state_update() {
    let server = TestServer::start([127, 0, 0, 1], content(1000)).await;
    let h = Harness::new(loopback_policy(), |_| {}).await;
    h.shutdown_dispatcher_for_manual_control().await;

    // Simulate: a previous run moved the file into place and recorded
    // file_path, then crashed before marking the download completed.
    let d = h.insert_raw(&server.url("/files/a.bin")).await;
    let claimed = h.store.claim_next(Utc::now()).await.unwrap().unwrap();
    let final_path = h.download_dir.join("a.bin");
    std::fs::write(&final_path, &server.state.content).unwrap();
    h.store
        .update_transfer_info(
            d.id,
            &crate::store::TransferInfo {
                total_bytes: Some(1000),
                downloaded_bytes: 1000,
                ..Default::default()
            },
            Utc::now(),
        )
        .await
        .unwrap();
    h.store
        .set_file_path(d.id, &final_path.to_string_lossy(), Utc::now())
        .await
        .unwrap();
    h.store.recover_interrupted(Utc::now()).await.unwrap();
    assert_eq!(claimed.id, d.id);

    let engine = h.restart_engine();
    let d = h.wait_state(d.id, TransferState::Completed).await;
    assert_eq!(server.hits(), 0, "nothing re-downloaded");
    assert_eq!(d.file_path.unwrap(), final_path.to_string_lossy());
    assert_eq!(std::fs::read_dir(&h.download_dir).unwrap().count(), 1);
    engine.0.shutdown(engine.1).await;
}

// --- SSRF -----------------------------------------------------------------

#[tokio::test]
async fn private_ip_literals_are_blocked_at_connect_time() {
    let server = TestServer::start([127, 0, 0, 1], content(10)).await;
    let h = Harness::new(NetworkPolicy::default(), |_| {}).await;

    // Rejected at submission...
    let err = h
        .service
        .submit(&server.url("/files/a"), None)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidRequest);

    // ...and again by the engine if it somehow got into the queue.
    let d = h.insert_raw(&server.url("/files/a")).await;
    let d = h.wait_state(d.id, TransferState::Failed).await;
    assert_eq!(d.error.unwrap().kind, ErrorKind::InvalidRequest);
    assert_eq!(server.hits(), 0, "no request may reach the server");
}

#[tokio::test]
async fn hostnames_resolving_to_private_addresses_are_blocked() {
    let server = TestServer::start([127, 0, 0, 1], content(10)).await;
    let h = Harness::new(NetworkPolicy::default(), |_| {}).await;
    let url = format!("http://localhost:{}/files/a", server.addr.port());
    // Submission cannot know where a hostname resolves, so it is accepted...
    let d = h.service.submit(&url, None).await.unwrap();
    // ...and blocked by the resolver at connection time.
    let d = h.wait_state(d.id, TransferState::Failed).await;
    assert_eq!(d.error.unwrap().kind, ErrorKind::InvalidRequest);
    assert_eq!(d.retry_count, 0, "policy violations are not retried");
    assert_eq!(server.hits(), 0);
}

#[tokio::test]
async fn redirects_to_forbidden_addresses_are_blocked() {
    // Only 127.0.0.2 is allowed. The origin there redirects to a server on
    // 127.0.0.1, both by IP literal and by a hostname resolving to it; the
    // target must never be contacted.
    let origin = TestServer::start([127, 0, 0, 2], content(10)).await;
    let target = TestServer::start([127, 0, 0, 1], content(10)).await;
    let policy = NetworkPolicy::new(vec!["127.0.0.2/32".parse().unwrap()]);
    let h = Harness::new(policy, |_| {}).await;

    let to_ip = format!("/redirect?to={}", target.url("/files/a"));
    let to_name = format!(
        "/redirect?to=http://localhost:{}/files/a",
        target.addr.port()
    );
    for path in [to_ip, to_name] {
        let d = h.service.submit(&origin.url(&path), None).await.unwrap();
        let d = h.wait_state(d.id, TransferState::Failed).await;
        assert_eq!(d.error.unwrap().kind, ErrorKind::InvalidRequest, "{path}");
    }
    assert_eq!(target.hits(), 0);

    // A redirect to an allowed destination works.
    let d = h
        .service
        .submit(
            &origin.url(&format!("/redirect?to={}", origin.url("/files/ok.bin"))),
            None,
        )
        .await
        .unwrap();
    let d = h.wait_state(d.id, TransferState::Completed).await;
    assert_eq!(file_contents(&d), origin.state.content);
}

/// Opt-in: `cargo test -- --ignored real_https` downloads a small file from
/// the internet to verify TLS and certificate validation end to end.
#[tokio::test]
#[ignore = "requires internet access"]
async fn real_https_download() {
    let h = Harness::new(NetworkPolicy::default(), |_| {}).await;
    let d = h
        .service
        .submit(
            "https://www.rust-lang.org/static/images/rust-logo-blk.svg",
            None,
        )
        .await
        .unwrap();
    let d = h
        .wait_for(d.id, |d| {
            d.state != TransferState::Queued && d.state != TransferState::Downloading
        })
        .await;
    assert_eq!(d.state, TransferState::Completed, "{:?}", d.error);
    assert!(file_contents(&d).starts_with(b"<svg") || file_contents(&d).starts_with(b"<?xml"));
}

// --- 1fichier ---------------------------------------------------------------

const TEST_API_KEY: &str = "TestKey_0123456789abcdefghijklmn";
const FILE_LINK: &str = "https://1fichier.com/?abcde12345";

async fn onefichier_harness(server: &TestServer, key: Option<&str>, interval: Duration) -> Harness {
    let api_base = server.api_base();
    let key = key.map(crate::config::Secret::new);
    Harness::with_providers(
        loopback_policy(),
        |_| {},
        move |client| standard_test_providers(client, Some((api_base, key, interval))),
    )
    .await
}

#[tokio::test]
async fn onefichier_premium_download() {
    let server = TestServer::start([127, 0, 0, 1], content(300_000)).await;
    let h = onefichier_harness(&server, Some(TEST_API_KEY), Duration::ZERO).await;
    let d = h
        .service
        .submit("http://www.1fichier.com/?abcde12345&af=42", None)
        .await
        .unwrap();
    assert_eq!(d.provider, "1fichier");
    assert_eq!(d.display_url, "https://1fichier.com/?…");

    let d = h.wait_state(d.id, TransferState::Completed).await;
    assert_eq!(file_contents(&d), server.state.content);
    assert_eq!(d.filename.as_deref(), Some("Movie.2024.mkv"));

    let calls = server.api_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].authorization.as_deref(),
        Some(format!("Bearer {TEST_API_KEY}").as_str())
    );
    assert_eq!(calls[0].body, serde_json::json!({"url": FILE_LINK}));
}

#[tokio::test]
async fn onefichier_password_required_then_supplied() {
    let server = TestServer::start([127, 0, 0, 1], content(1000)).await;
    server.script_api(
        403,
        serde_json::json!({"status": "KO", "message": "Password not provided. Resource not allowed #632"}),
    );
    let h = onefichier_harness(&server, Some(TEST_API_KEY), Duration::ZERO).await;
    let d = h.service.submit(FILE_LINK, None).await.unwrap();
    let d = h.wait_state(d.id, TransferState::Blocked).await;
    let err = d.error.unwrap();
    assert_eq!(err.kind, ErrorKind::AuthenticationFailed);
    assert!(err.message.contains("password"), "{}", err.message);

    h.service.retry(d.id, Some("s3cret pass")).await.unwrap();
    let d = h.wait_state(d.id, TransferState::Completed).await;
    assert_eq!(file_contents(&d), server.state.content);
    let calls = server.api_calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].body.get("pass").is_none());
    assert_eq!(calls[1].body["pass"], "s3cret pass");

    let events = h.service.events(d.id).await.unwrap();
    assert!(events.iter().all(|e| !e.message.contains("s3cret")));
}

#[tokio::test]
async fn onefichier_permanent_errors_are_not_retried() {
    for (status, message, expected_state, expected_kind) in [
        (
            401,
            "Not authenticated #247",
            TransferState::Blocked,
            ErrorKind::AuthenticationFailed,
        ),
        (
            403,
            "Must be a customer (Premium, Access) #200",
            TransferState::Blocked,
            ErrorKind::AuthenticationFailed,
        ),
        (
            404,
            "Resource not found #469",
            TransferState::Failed,
            ErrorKind::RemoteUnavailable,
        ),
        (
            403,
            "Resource not allowed #634",
            TransferState::Failed,
            ErrorKind::RemoteUnavailable,
        ),
    ] {
        let server = TestServer::start([127, 0, 0, 1], content(10)).await;
        server.script_api(
            status,
            serde_json::json!({"status": "KO", "message": message}),
        );
        let h = onefichier_harness(&server, Some(TEST_API_KEY), Duration::ZERO).await;
        let d = h.service.submit(FILE_LINK, None).await.unwrap();
        let d = h.wait_state(d.id, expected_state).await;
        assert_eq!(d.error.unwrap().kind, expected_kind, "{message}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            server.api_calls().len(),
            1,
            "no automatic retry for {message}"
        );
        assert_eq!(server.hits(), 0, "no file request for {message}");
    }
}

#[tokio::test]
async fn onefichier_flood_protection_waits_minutes() {
    let server = TestServer::start([127, 0, 0, 1], content(10)).await;
    server.script_api(
        403,
        serde_json::json!({"status": "KO", "message": "Flood detected: IP Locked #38"}),
    );
    let h = onefichier_harness(&server, Some(TEST_API_KEY), Duration::ZERO).await;
    let before = Utc::now();
    let d = h.service.submit(FILE_LINK, None).await.unwrap();
    let d = h.wait_state(d.id, TransferState::Retrying).await;
    assert_eq!(d.error.unwrap().kind, ErrorKind::ProviderRestricted);
    let wait = d.next_attempt_at.unwrap() - before;
    assert!(
        wait >= chrono::Duration::minutes(4),
        "waits ~5 min, got {wait}"
    );
}

#[tokio::test]
async fn onefichier_without_api_key_blocks() {
    let server = TestServer::start([127, 0, 0, 1], content(10)).await;
    let h = onefichier_harness(&server, None, Duration::ZERO).await;
    let d = h.service.submit(FILE_LINK, None).await.unwrap();
    let d = h.wait_state(d.id, TransferState::Blocked).await;
    assert!(d.error.unwrap().message.contains("API key"));
    assert!(server.api_calls().is_empty());
}

#[tokio::test]
async fn onefichier_resume_uses_a_fresh_token_and_size_check() {
    let server = TestServer::start([127, 0, 0, 1], content(2_000_000)).await;
    server.set_delay(Duration::from_millis(5));
    let h = onefichier_harness(&server, Some(TEST_API_KEY), Duration::ZERO).await;
    let d = h.service.submit(FILE_LINK, None).await.unwrap();
    h.wait_progress(d.id, 100_000).await;
    h.service.pause(d.id).await.unwrap();
    let partial = std::fs::metadata(h.part_path(d.id)).unwrap().len();

    server.set_delay(Duration::ZERO);
    h.service.resume(d.id).await.unwrap();
    let d = h.wait_state(d.id, TransferState::Completed).await;
    assert_eq!(file_contents(&d), server.state.content);
    assert_eq!(server.api_calls().len(), 2, "a new token per attempt");
    assert_eq!(
        server.ranges().last().unwrap().as_deref(),
        Some(format!("bytes={partial}-").as_str())
    );
    assert_eq!(
        server.if_ranges().last().unwrap(),
        &None,
        "no validator needed"
    );
}

#[tokio::test]
async fn onefichier_resume_restarts_when_size_differs() {
    let server = TestServer::start([127, 0, 0, 1], content(2_000_000)).await;
    server.set_delay(Duration::from_millis(5));
    let h = onefichier_harness(&server, Some(TEST_API_KEY), Duration::ZERO).await;
    let d = h.service.submit(FILE_LINK, None).await.unwrap();
    h.wait_progress(d.id, 100_000).await;
    h.service.pause(d.id).await.unwrap();

    server.set_delay(Duration::ZERO);
    server.state.wrong_total_next.store(true, Ordering::SeqCst);
    h.service.resume(d.id).await.unwrap();
    let d = h.wait_state(d.id, TransferState::Completed).await;
    assert_eq!(file_contents(&d), server.state.content);
    let ranges = server.ranges();
    let n = ranges.len();
    assert!(ranges[n - 2].is_some(), "tried to resume");
    assert_eq!(ranges[n - 1], None, "then restarted from zero");
}

#[tokio::test]
async fn onefichier_html_error_page_is_not_saved() {
    let server = TestServer::start([127, 0, 0, 1], content(10)).await;
    server.script_api(
        200,
        serde_json::json!({"status": "OK", "url": server.url("/html")}),
    );
    let h = onefichier_harness(&server, Some(TEST_API_KEY), Duration::ZERO).await;
    let d = h.service.submit(FILE_LINK, None).await.unwrap();
    let d = h.wait_state(d.id, TransferState::Retrying).await;
    assert!(d.error.unwrap().message.contains("web page"));
    assert!(!h.part_path(d.id).exists());
    assert_eq!(std::fs::read_dir(&h.download_dir).unwrap().count(), 0);
}

#[tokio::test]
async fn onefichier_api_calls_are_spaced() {
    let server = TestServer::start([127, 0, 0, 1], content(1000)).await;
    let interval = Duration::from_millis(300);
    let h = Harness::with_providers(loopback_policy(), |s| s.max_concurrent = 3, {
        let api_base = server.api_base();
        move |client| {
            standard_test_providers(
                client,
                Some((
                    api_base,
                    Some(crate::config::Secret::new(TEST_API_KEY)),
                    interval,
                )),
            )
        }
    })
    .await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(h.service.submit(FILE_LINK, None).await.unwrap().id);
    }
    for id in ids {
        h.wait_state(id, TransferState::Completed).await;
    }
    let calls = server.api_calls();
    assert_eq!(calls.len(), 3);
    for pair in calls.windows(2) {
        let gap = pair[1].at - pair[0].at;
        assert!(gap >= Duration::from_millis(280), "API calls {gap:?} apart");
    }
}

#[tokio::test]
async fn submission_validates_links_and_passwords() {
    let h = Harness::new(loopback_policy(), |_| {}).await;
    let err = h
        .service
        .submit("https://1fichier.com/?SHOUT", None)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidRequest);
    let err = h
        .service
        .submit("https://example.com/file.bin", Some("pw"))
        .await
        .unwrap_err();
    assert!(err.message.contains("does not use a download password"));
    let err = h
        .service
        .submit(FILE_LINK, Some(&"x".repeat(300)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidRequest);
    // An empty password means none.
    assert!(
        h.service
            .submit(FILE_LINK, Some(""))
            .await
            .unwrap()
            .download_password
            .is_none()
    );
}

// --- harness helpers for crash simulation ---------------------------------

impl Harness {
    async fn shutdown_dispatcher_for_manual_control(&self) {
        // Stop claiming: the test drives the store directly, then restarts.
        self.engine.inner.shutdown.cancel();
        // The dispatcher exits promptly; nothing is running yet.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    fn restart_engine(&self) -> (Engine, JoinHandle<()>) {
        let engine = Engine::new(
            self.store.clone(),
            self.providers.clone(),
            crate::http_client::build_client(loopback_policy()).unwrap(),
            loopback_policy(),
            self.engine.inner.settings.clone(),
        );
        let handle = engine.start();
        (engine, handle)
    }
}
