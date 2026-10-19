//! A very small HTTP/1.1 server that lets Prometheus collect our metrics. It
//! uses tokio TCP and no HTTP library. It has one route: `GET /metrics`
//! returns the text from `render_prometheus`, or from the function a program
//! that runs the engine hands to `serve_page`. Each connection answers one
//! request and then closes (`Connection: close`). Collecting metrics needs
//! nothing more than a single GET, so there is no routing, no keep-alive and
//! no reading of request bodies.
//!
//! The endpoint is read-only and has no authentication. Bind it to an address
//! you trust, such as 127.0.0.1 or a monitoring network.

use crate::metrics::{render_prometheus, Metrics};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

const PROM_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";
const MAX_REQUEST_BYTES: usize = 8192;
/// How long one connection may take from start to finish.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// How many connections may be open at once.
const MAX_CONNECTIONS: usize = 64;

/// Makes the text of the page, each time it is asked for.
pub type Render = Arc<dyn Fn() -> String + Send + Sync>;

/// Serves the engine's own numbers on `listener` until `cancel` fires.
pub(crate) async fn serve_metrics(
    listener: TcpListener,
    metrics: Arc<Metrics>,
    cancel: CancellationToken,
) {
    serve_page(listener, Arc::new(move || render_prometheus(&metrics.snapshot())), cancel).await
}

/// Serves `GET /metrics` on `listener` until `cancel` fires, answering with
/// what `render` gives. A program that runs the engine uses it to show its
/// own numbers next to the engine's. Each connection gets its own task, so
/// one slow client cannot hold up new connections.
pub async fn serve_page(listener: TcpListener, render: Render, cancel: CancellationToken) {
    serve(listener, render, cancel, REQUEST_TIMEOUT, MAX_CONNECTIONS).await
}

/// The loop behind `serve_metrics`, with its two limits passed in so tests can
/// use small ones.
///
/// A connection gets `request_timeout` to send its request and take the
/// answer, and is then closed. At most `max_conns` connections are open at
/// once, and one that arrives beyond that is closed straight away. Without
/// these, clients that connect and send nothing would each hold a task and a
/// socket for as long as they liked. When `cancel` fires, open connections are
/// closed too.
async fn serve(
    listener: TcpListener,
    render: Render,
    cancel: CancellationToken,
    request_timeout: std::time::Duration,
    max_conns: usize,
) {
    let places = Arc::new(tokio::sync::Semaphore::new(max_conns));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                // No place free: dropping the stream closes the connection.
                let Ok(place) = places.clone().try_acquire_owned() else { continue };
                let render = render.clone();
                let cancel = cancel.clone();
                tokio::spawn(async move {
                    let _place = place;
                    tokio::select! {
                        _ = cancel.cancelled() => {}
                        _ = tokio::time::timeout(request_timeout, handle_conn(stream, &render)) => {}
                    }
                });
            }
        }
    }
}

async fn handle_conn(mut stream: TcpStream, render: &Render) {
    // Read the request headers up to the blank line (CRLF CRLF) that ends
    // them. Stop early once more than `MAX_REQUEST_BYTES` have arrived.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if find_header_end(&buf) || buf.len() > MAX_REQUEST_BYTES {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let first_line = head.lines().next().unwrap_or("");
    let resp = match parse_request_line(first_line) {
        Some(("GET", "/metrics")) => {
            let body = render();
            response("200 OK", Some(PROM_CONTENT_TYPE), &body)
        }
        Some(("GET", _)) => response("404 Not Found", None, ""),
        Some((_, _)) => response("405 Method Not Allowed", None, ""),
        None => response("400 Bad Request", None, ""),
    };
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
}

fn find_header_end(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"\r\n\r\n")
}

/// Splits an HTTP request line, "METHOD PATH VERSION", into (method, path).
fn parse_request_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?;
    Some((method, path))
}

/// Builds an HTTP/1.1 response with `Content-Length` and `Connection: close`
/// headers.
fn response(status_line: &str, content_type: Option<&str>, body: &str) -> String {
    let mut out = format!("HTTP/1.1 {status_line}\r\n");
    if let Some(ct) = content_type {
        out.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n", body.len()));
    out.push_str("Connection: close\r\n\r\n");
    out.push_str(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    /// Starts the server on a free port with the given limits.
    async fn start(request_timeout: Duration, max_conns: usize) -> (String, CancellationToken) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let cancel = CancellationToken::new();
        let metrics = Arc::new(Metrics::default());
        let render: Render = Arc::new(move || render_prometheus(&metrics.snapshot()));
        tokio::spawn(serve(listener, render, cancel.clone(), request_timeout, max_conns));
        (addr, cancel)
    }

    /// True if the server closes `stream` within two seconds.
    async fn is_closed(stream: &mut TcpStream) -> bool {
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf)).await.is_ok()
    }

    #[tokio::test]
    async fn a_client_that_sends_nothing_is_dropped_after_the_time_limit() {
        let (addr, _cancel) = start(Duration::from_millis(200), 8).await;
        let mut silent = TcpStream::connect(&addr).await.unwrap();
        assert!(is_closed(&mut silent).await, "a silent client was kept open");
    }

    #[tokio::test]
    async fn connections_over_the_limit_are_closed_at_once() {
        let (addr, _cancel) = start(Duration::from_secs(30), 2).await;
        let _first = TcpStream::connect(&addr).await.unwrap();
        let _second = TcpStream::connect(&addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut third = TcpStream::connect(&addr).await.unwrap();
        assert!(is_closed(&mut third).await, "a third connection got a place");
    }

    #[tokio::test]
    async fn shutting_down_closes_open_connections() {
        let (addr, cancel) = start(Duration::from_secs(30), 8).await;
        let mut silent = TcpStream::connect(&addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
        assert!(is_closed(&mut silent).await, "the connection outlived the shutdown");
    }

    // A program that runs the engine can serve a page of its own, with its
    // own numbers next to the engine's.
    #[tokio::test]
    async fn a_page_shows_what_it_is_given() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cancel = CancellationToken::new();
        let render: crate::Render = Arc::new(|| "orvu_example 7\n".to_string());
        tokio::spawn(crate::serve_page(listener, render, cancel.clone()));
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(answer.ends_with("\r\n\r\norvu_example 7\n"), "{answer}");
        cancel.cancel();
    }

    #[test]
    fn parse_request_line_extracts_method_and_path() {
        assert_eq!(parse_request_line("GET /metrics HTTP/1.1"), Some(("GET", "/metrics")));
        assert_eq!(parse_request_line("POST /x HTTP/1.1"), Some(("POST", "/x")));
        assert_eq!(parse_request_line(""), None);
        assert_eq!(parse_request_line("GET"), None); // no path
    }

    #[test]
    fn response_is_well_formed() {
        let r = response("200 OK", Some("text/plain"), "hello");
        assert!(r.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(r.contains("Content-Type: text/plain\r\n"));
        assert!(r.contains("Content-Length: 5\r\n"));
        assert!(r.contains("Connection: close\r\n"));
        assert!(r.ends_with("\r\n\r\nhello"));
    }

    #[test]
    fn response_omits_content_type_when_none() {
        let r = response("404 Not Found", None, "");
        assert!(r.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(!r.contains("Content-Type"));
        assert!(r.contains("Content-Length: 0\r\n"));
    }
}
