//! A single-route HTTP server for Prometheus scrapes.
//!
//! Bind it to a private address: it has no authentication, and the metrics
//! reveal your topology.

use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    time,
};
use tracing::{debug, info, warn};

use crate::{Collector, Metrics};

/// Request headers must arrive within this window and fit in this many bytes.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_BYTES: usize = 8 * 1024;

pub struct Exporter {
    pub metrics: Arc<Metrics>,
    pub collector: Option<Arc<dyn Collector>>,
}

impl Exporter {
    pub fn new(metrics: Arc<Metrics>) -> Self {
        Self { metrics, collector: None }
    }

    pub fn with_collector(mut self, collector: Arc<dyn Collector>) -> Self {
        self.collector = Some(collector);
        self
    }

    fn render(&self) -> String {
        self.metrics.render(self.collector.as_deref())
    }
}

/// Serves until `shutdown` flips.
pub async fn serve(
    bind: SocketAddr,
    exporter: Arc<Exporter>,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!(address = %listener.local_addr()?, "metrics endpoint listening");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        let exporter = Arc::clone(&exporter);
                        tokio::spawn(async move {
                            if let Err(err) = handle(stream, &exporter).await {
                                debug!(%peer, %err, "metrics request failed");
                            }
                        });
                    }
                    Err(err) => {
                        warn!(%err, "metrics accept failed");
                        time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    info!("metrics endpoint stopping");
                    return Ok(());
                }
            }
        }
    }
}

async fn handle(mut stream: TcpStream, exporter: &Exporter) -> io::Result<()> {
    let request = time::timeout(REQUEST_TIMEOUT, read_request_line(&mut stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "request timed out"))??;

    let mut parts = request.split(' ');
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    // Ignore any query string; this endpoint has no parameters.
    let path = path.split('?').next().unwrap_or(path);

    let response = match (method, path) {
        ("GET" | "HEAD", "/metrics") => {
            respond(200, "text/plain; version=0.0.4; charset=utf-8", &exporter.render())
        }
        ("GET" | "HEAD", "/health" | "/healthz" | "/-/healthy") => {
            respond(200, "text/plain; charset=utf-8", "ok\n")
        }
        ("GET" | "HEAD", "/") => respond(
            200,
            "text/html; charset=utf-8",
            "<html><body><a href=\"/metrics\">metrics</a></body></html>\n",
        ),
        ("GET" | "HEAD", _) => respond(404, "text/plain; charset=utf-8", "not found\n"),
        _ => respond(405, "text/plain; charset=utf-8", "method not allowed\n"),
    };

    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// Reads until the end of the headers, returning the request line.
async fn read_request_line(stream: &mut TcpStream) -> io::Result<String> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 512];

    loop {
        if let Some(end) = find_header_end(&buf) {
            let head = String::from_utf8_lossy(&buf[..end]);
            return Ok(head.lines().next().unwrap_or_default().trim().to_owned());
        }
        if buf.len() > MAX_REQUEST_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request headers too large"));
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "client closed the request"));
        }
        buf.extend_from_slice(&chunk[..read]);
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .or_else(|| buf.windows(2).position(|w| w == b"\n\n"))
}

fn respond(status: u16, content_type: &str, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn request(addr: SocketAddr, raw: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(raw.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    async fn start() -> (SocketAddr, watch::Sender<bool>, tokio::task::JoinHandle<()>) {
        let metrics = Metrics::new();
        metrics.connection_opened();
        metrics.status_requests.increment("gateway");

        // Bind first so the test knows the port before the server task starts.
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let (tx, rx) = watch::channel(false);
        let exporter = Arc::new(Exporter::new(metrics));
        let handle = tokio::spawn(async move {
            serve(addr, exporter, rx).await.unwrap();
        });
        // Give the listener a moment to come up.
        for _ in 0..50 {
            if TcpStream::connect(addr).await.is_ok() {
                break;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        (addr, tx, handle)
    }

    #[tokio::test]
    async fn serves_the_metrics_endpoint() {
        let (addr, stop, handle) = start().await;
        let response = request(addr, "GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n").await;

        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("text/plain; version=0.0.4"), "{response}");
        assert!(response.contains("mc_gateway_connections_total 1"), "{response}");
        assert!(
            response.contains(r#"mc_gateway_status_requests_total{source="gateway"} 1"#),
            "{response}"
        );

        stop.send(true).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn serves_health_and_rejects_the_rest() {
        let (addr, stop, handle) = start().await;

        assert!(
            request(addr, "GET /healthz HTTP/1.1\r\n\r\n").await.starts_with("HTTP/1.1 200 OK")
        );
        assert!(
            request(addr, "GET /secrets HTTP/1.1\r\n\r\n")
                .await
                .starts_with("HTTP/1.1 404")
        );
        assert!(
            request(addr, "POST /metrics HTTP/1.1\r\n\r\n")
                .await
                .starts_with("HTTP/1.1 405")
        );
        // A query string must not change the route.
        assert!(
            request(addr, "GET /metrics?foo=bar HTTP/1.1\r\n\r\n")
                .await
                .contains("mc_gateway_connections_total")
        );

        stop.send(true).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn an_oversized_request_is_dropped() {
        let (addr, stop, handle) = start().await;

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let junk = "x".repeat(MAX_REQUEST_BYTES + 1024);
        // No header terminator, so the server must bail on size, not buffer on.
        let _ = stream.write_all(format!("GET /metrics {junk}").as_bytes()).await;
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response).await;
        assert!(response.is_empty(), "the server closed without answering");

        // The exporter is still healthy afterwards.
        assert!(request(addr, "GET /healthz HTTP/1.1\r\n\r\n").await.contains("200 OK"));

        stop.send(true).unwrap();
        handle.await.unwrap();
    }
}
