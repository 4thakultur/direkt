//! The HTTP server loop.
//!
//! `axum::serve` sets no header-read timeout and accepts unlimited
//! connections, so a client could hold thousands of connections open by
//! sending headers slowly ("slowloris") until Direkt runs out of file
//! descriptors. This loop serves the same router with:
//! * a header-read timeout,
//! * a cap on simultaneous connections (the accept loop waits when full),
//! * graceful shutdown with a bounded grace period.

use axum::Router;
use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::server::graceful::GracefulShutdown;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower::ServiceExt;

/// Time a client gets to send a complete request head.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// A connection without any traffic for this long is closed. Longer than any
/// request takes to handle (pausing a transfer waits at most 30 s).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Simultaneous connections; plenty for a single-user service.
pub const MAX_CONNECTIONS: usize = 512;
/// Simultaneous connections from one address (IPv6: one /64).
pub const MAX_CONNECTIONS_PER_CLIENT: usize = 64;
/// How long shutdown waits for in-flight requests.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Serve `router` on `listener` until `shutdown` resolves. Each request
/// carries the peer address as `ConnectInfo<SocketAddr>`.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()>,
) -> std::io::Result<()> {
    serve_with_limits(
        listener,
        router,
        shutdown,
        Limits {
            header_read: HEADER_READ_TIMEOUT,
            idle: IDLE_TIMEOUT,
            connections: MAX_CONNECTIONS,
            per_client: MAX_CONNECTIONS_PER_CLIENT,
        },
    )
    .await
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    pub header_read: Duration,
    pub idle: Duration,
    pub connections: usize,
    pub per_client: usize,
}

pub(crate) async fn serve_with_limits(
    listener: TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()>,
    limits: Limits,
) -> std::io::Result<()> {
    let header_read_timeout = limits.header_read;
    let max_connections = limits.connections;
    let per_client = Arc::new(PerClient::default());
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    let builder = Arc::new(builder);
    let slots = Arc::new(Semaphore::new(max_connections));
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);

    loop {
        let permit = tokio::select! {
            () = &mut shutdown => break,
            permit = slots.clone().acquire_owned() => permit.expect("semaphore is never closed"),
        };
        let (stream, peer) = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(e) => {
                    // Typically EMFILE or a reset during accept: back off briefly.
                    tracing::warn!(error = %e, "failed to accept connection");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        let Some(client_slot) = per_client.acquire(peer.ip(), limits.per_client) else {
            tracing::warn!(%peer, "too many connections from one client; refusing");
            continue;
        };
        let stream = IdleTimeout::new(stream, limits.idle);
        let router = router.clone();
        let service = hyper::service::service_fn(move |mut request: hyper::Request<Incoming>| {
            request.extensions_mut().insert(ConnectInfo(peer));
            router.clone().oneshot(request)
        });
        let connection = builder
            .serve_connection_with_upgrades(TokioIo::new(stream), service)
            .into_owned();
        let connection = graceful.watch(connection);
        tokio::spawn(async move {
            let _permit = permit;
            let _client_slot = client_slot;
            if let Err(e) = connection.await {
                tracing::debug!(error = %e, %peer, "connection ended with an error");
            }
        });
    }

    drop(listener);
    if tokio::time::timeout(SHUTDOWN_GRACE, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("some HTTP connections did not close in time");
    }
    Ok(())
}

/// Ask a running Direkt listening on `listen` whether it is healthy, the way
/// the container health check does (the runtime image has no curl). An
/// unspecified listen address (`0.0.0.0`, `::`) is probed on loopback.
pub async fn probe_health(listen: std::net::SocketAddr) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut addr = listen;
    if addr.ip().is_unspecified() {
        addr.set_ip(match addr {
            std::net::SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
            std::net::SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
        });
    }
    let probe = async {
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut head = Vec::new();
        stream.take(4096).read_to_end(&mut head).await?;
        Ok::<_, std::io::Error>(head)
    };
    let response = tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .map_err(|_| format!("{addr}: no answer within 5 s"))?
        .map_err(|e| format!("{addr}: {e}"))?;
    let status_line = response.split(|&b| b == b'\n').next().unwrap_or_default();
    let status_line = String::from_utf8_lossy(status_line);
    match status_line.split_whitespace().nth(1) {
        Some("200") => Ok(()),
        _ => Err(format!("{addr}: unhealthy: {}", status_line.trim())),
    }
}

/// Open connections per client, keyed like the login limiter (IPv6 by /64).
#[derive(Default)]
struct PerClient {
    open: std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, usize>>,
}

impl PerClient {
    fn acquire(self: &Arc<Self>, ip: std::net::IpAddr, max: usize) -> Option<ClientSlot> {
        let key = crate::auth::limiter_key(ip);
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let count = open.entry(key).or_insert(0);
        if *count >= max {
            return None;
        }
        *count += 1;
        Some(ClientSlot {
            owner: self.clone(),
            key,
        })
    }
}

struct ClientSlot {
    owner: Arc<PerClient>,
    key: std::net::IpAddr,
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        let mut open = self.owner.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = open.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                open.remove(&self.key);
            }
        }
    }
}

/// A stream that fails with `TimedOut` after `timeout` without progress in
/// either direction.
struct IdleTimeout<S> {
    inner: S,
    timeout: Duration,
    deadline: std::pin::Pin<Box<tokio::time::Sleep>>,
}

impl<S> IdleTimeout<S> {
    fn new(inner: S, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            deadline: Box::pin(tokio::time::sleep(timeout)),
        }
    }

    fn progressed(&mut self) {
        let next = tokio::time::Instant::now() + self.timeout;
        self.deadline.as_mut().reset(next);
    }

    fn expired(&mut self, cx: &mut std::task::Context<'_>) -> bool {
        self.deadline.as_mut().poll(cx).is_ready()
    }
}

fn timed_out() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, "connection idle for too long")
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for IdleTimeout<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::task::Poll;
        match std::pin::Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(result) => {
                self.progressed();
                Poll::Ready(result)
            }
            Poll::Pending if self.expired(cx) => Poll::Ready(Err(timed_out())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for IdleTimeout<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::task::Poll;
        match std::pin::Pin::new(&mut self.inner).poll_write(cx, data) {
            Poll::Ready(result) => {
                self.progressed();
                Poll::Ready(result)
            }
            Poll::Pending if self.expired(cx) => Poll::Ready(Err(timed_out())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use std::net::SocketAddr;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn start(
        header_timeout: Duration,
        max: usize,
    ) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
        start_with(Limits {
            header_read: header_timeout,
            idle: header_timeout,
            connections: max,
            per_client: max,
        })
        .await
    }

    async fn start_with(limits: Limits) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
        let router = Router::new().route(
            "/",
            get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.ip().to_string() }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(serve_with_limits(
            listener,
            router,
            async {
                let _ = stopped.await;
            },
            limits,
        ));
        (addr, stop)
    }

    async fn get_root(addr: SocketAddr) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn serves_requests_with_peer_address() {
        let (addr, _stop) = start(Duration::from_secs(5), 8).await;
        let response = get_root(addr).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("127.0.0.1"), "{response}");
    }

    #[tokio::test]
    async fn slow_headers_are_cut_off() {
        let (addr, _stop) = start(Duration::from_millis(300), 8).await;
        let mut slow = tokio::net::TcpStream::connect(addr).await.unwrap();
        slow.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n")
            .await
            .unwrap();
        let started = Instant::now();
        let mut buf = Vec::new();
        // The server closes the connection (possibly after a 408) instead of
        // waiting forever for the rest of the head.
        let _ = tokio::time::timeout(Duration::from_secs(5), slow.read_to_end(&mut buf))
            .await
            .expect("connection must be closed by the server");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn connections_beyond_the_cap_wait() {
        let (addr, _stop) = start(Duration::from_secs(2), 2).await;
        // Two idle connections take both slots.
        let _a = tokio::net::TcpStream::connect(addr).await.unwrap();
        let _b = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let blocked = tokio::time::timeout(Duration::from_millis(500), get_root(addr)).await;
        assert!(
            blocked.is_err(),
            "a third connection is not served while the cap is reached"
        );
        // Once the idle ones time out, service resumes.
        let response = tokio::time::timeout(Duration::from_secs(5), get_root(addr))
            .await
            .expect("served after slots free up");
        assert!(response.starts_with("HTTP/1.1 200"));
    }

    #[tokio::test]
    async fn silent_connections_are_closed() {
        let (addr, _stop) = start(Duration::from_millis(300), 8).await;
        let mut silent = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), silent.read_to_end(&mut buf))
            .await
            .expect("a connection that never sends a byte is closed")
            .ok();
    }

    #[tokio::test]
    async fn connections_per_client_are_capped() {
        let (addr, _stop) = start_with(Limits {
            header_read: Duration::from_secs(5),
            idle: Duration::from_secs(5),
            connections: 16,
            per_client: 2,
        })
        .await;
        let _a = tokio::net::TcpStream::connect(addr).await.unwrap();
        let _b = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The third connection from the same address is closed at once
        // (reset or EOF, never a response).
        let attempt = async {
            let mut stream = tokio::net::TcpStream::connect(addr).await?;
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                .await?;
            let mut response = String::new();
            stream.read_to_string(&mut response).await?;
            Ok::<_, std::io::Error>(response)
        };
        let result = tokio::time::timeout(Duration::from_secs(2), attempt)
            .await
            .expect("refused promptly, not left hanging");
        assert!(
            result.is_err() || result.as_ref().unwrap().is_empty(),
            "{result:?}"
        );
    }

    async fn health_server(status: axum::http::StatusCode) -> SocketAddr {
        let router = Router::new().route("/healthz", get(move || async move { status }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, router, std::future::pending()));
        addr
    }

    #[tokio::test]
    async fn health_probe_reports_status() {
        let ok = health_server(axum::http::StatusCode::OK).await;
        assert_eq!(probe_health(ok).await, Ok(()));
        // Probed on loopback when listening on all interfaces.
        let any = SocketAddr::from(([0, 0, 0, 0], ok.port()));
        assert_eq!(probe_health(any).await, Ok(()));

        let down = health_server(axum::http::StatusCode::SERVICE_UNAVAILABLE).await;
        let err = probe_health(down).await.unwrap_err();
        assert!(err.contains("503"), "{err}");
    }

    #[tokio::test]
    async fn health_probe_fails_when_nothing_listens() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert!(probe_health(addr).await.is_err());
    }

    #[tokio::test]
    async fn shuts_down() {
        let (addr, stop) = start(Duration::from_secs(5), 8).await;
        assert!(get_root(addr).await.starts_with("HTTP/1.1 200"));
        stop.send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(tokio::net::TcpStream::connect(addr).await.is_err());
    }
}
