//! Minimal read-only HTTP/1.1 server and client for `gtt.peer` over
//! `std::net` — no tokio/axum, the same approach as the OTLP receiver.
//!
//! GET only, one request per connection (`Connection: close`), one thread per
//! connection up to [`MAX_CONNECTIONS`], hard limits on the request head and
//! on how long a client may take to send it. Routes are
//! `/peer/v{N}/{hello|summary|quotas|sources}`. The `Host` header must be an
//! IP literal or `localhost`, so a web page cannot reach a loopback server
//! through DNS rebinding.

use super::{Identity, Request, SummaryQuery, failure, respond};
use anyhow::{Context, Result};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 4319;
/// Loopback only unless the user explicitly binds elsewhere.
pub const DEFAULT_BIND: &str = "127.0.0.1:4319";
/// Request line + headers (a summary's `known` list rides in the URL).
pub const MAX_HEAD: usize = 64 << 10;
pub const MAX_CONNECTIONS: usize = 16;
/// Total time a client gets to deliver its request head.
pub const HEAD_DEADLINE: Duration = Duration::from_secs(10);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Client-side cap on a response body.
const MAX_RESPONSE: usize = 256 << 20;

/// What the server answers with.
#[derive(Clone, Debug)]
pub struct ServerState {
    pub db: PathBuf,
    pub identity: Identity,
}

/// Serve forever on `listener` (blocking). Each accepted connection gets its
/// own thread; beyond [`MAX_CONNECTIONS`] concurrent ones the server answers
/// `503 server_busy` and closes.
pub fn serve(listener: &TcpListener, state: ServerState) -> Result<()> {
    let state = Arc::new(state);
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
            let _ = write_response(&mut stream, 503, &failure("request", "server_busy"));
            continue;
        }
        let (state, active) = (Arc::clone(&state), Arc::clone(&active));
        let spawned = std::thread::Builder::new()
            .name("gtt-peer-conn".into())
            .spawn({
                let active = Arc::clone(&active);
                move || {
                    let _ = handle(stream, &state);
                    active.fetch_sub(1, Ordering::SeqCst);
                }
            });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::SeqCst);
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, state: &ServerState) -> Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let (status, body) = match read_head(&mut stream) {
        Ok(head) => route(&head, state),
        Err(HeadError::TooLarge) => (431, failure("request", "request_too_large")),
        Err(HeadError::Timeout) => (408, failure("request", "request_timeout")),
        Err(HeadError::Closed) => return Ok(()),
    };
    write_response(&mut stream, status, &body)?;
    // Closing with unread input (a rejected body or oversized head) makes the
    // kernel send RST, which can destroy the response in flight: half-close,
    // then briefly discard what the client is still sending.
    stream.shutdown(Shutdown::Write)?;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = std::io::copy(
        &mut (&stream).take(MAX_HEAD as u64 * 4),
        &mut std::io::sink(),
    );
    Ok(())
}

enum HeadError {
    TooLarge,
    Timeout,
    Closed,
}

/// Read up to the blank line ending the head, within [`MAX_HEAD`] bytes and
/// [`HEAD_DEADLINE`]. Anything after it (a body) is never read.
fn read_head(stream: &mut TcpStream) -> std::result::Result<String, HeadError> {
    let deadline = Instant::now() + HEAD_DEADLINE;
    let mut head = Vec::with_capacity(1024);
    let mut buf = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(HeadError::Timeout);
        }
        let _ = stream.set_read_timeout(Some(left.min(IO_TIMEOUT)));
        let n = match stream.read(&mut buf) {
            Ok(0) => return Err(HeadError::Closed),
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => return Err(HeadError::Closed),
        };
        let scan_from = head.len().saturating_sub(3);
        head.extend_from_slice(&buf[..n]);
        if let Some(end) = head[scan_from..]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| scan_from + p)
        {
            if end > MAX_HEAD {
                return Err(HeadError::TooLarge);
            }
            head.truncate(end);
            return String::from_utf8(head).map_err(|_| HeadError::TooLarge);
        }
        if head.len() > MAX_HEAD {
            return Err(HeadError::TooLarge);
        }
    }
}

/// Map a request head to `(HTTP status, JSON body)`.
fn route(head: &str, state: &ServerState) -> (u16, Value) {
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split(' ');
    let (method, target, http) = (
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
    );
    if !http.starts_with("HTTP/1.") || parts.next().is_some() {
        return (400, failure("request", "invalid_request"));
    }
    if method != "GET" {
        return (405, failure("request", "method_not_allowed"));
    }
    let mut host_ok = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return (400, failure("request", "invalid_request"));
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") {
            host_ok = allowed_host(value);
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value != "0")
        {
            return (413, failure("request", "request_too_large"));
        }
    }
    if !host_ok {
        return (403, failure("request", "host_not_allowed"));
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let Some((version, endpoint)) = path
        .strip_prefix("/peer/v")
        .and_then(|rest| rest.split_once('/'))
    else {
        return (404, failure("request", "unknown_endpoint"));
    };
    let Ok(version) = version.parse::<u32>() else {
        return (404, failure("request", "unknown_endpoint"));
    };
    let request = match endpoint {
        "hello" => Request::Hello,
        "summary" => match SummaryQuery::parse(query) {
            Some(q) => Request::Summary(q),
            None => return (400, failure("summary", "invalid_request")),
        },
        "quotas" => Request::Quotas,
        "sources" => Request::Sources,
        _ => return (404, failure("request", "unknown_endpoint")),
    };
    if !matches!(request, Request::Summary(_)) && !query.is_empty() {
        return (400, failure(request.operation(), "invalid_request"));
    }
    if version != super::PROTOCOL_VERSION {
        return (
            400,
            failure(request.operation(), "unsupported_protocol_version"),
        );
    }
    // Ledger-level outcomes (busy, missing, disabled …) are 200 with
    // `status: unavailable` and a fixed `error.code`, like coordination.
    (200, respond(&state.db, &state.identity, version, &request))
}

/// `Host` must be an IP literal (optionally with port) or `localhost`.
pub(crate) fn allowed_host(host: &str) -> bool {
    if host.parse::<SocketAddr>().is_ok() || host.parse::<IpAddr>().is_ok() {
        return true;
    }
    let name = host
        .rsplit_once(':')
        .filter(|(_, port)| port.parse::<u16>().is_ok())
        .map_or(host, |(name, _)| name);
    name.eq_ignore_ascii_case("localhost")
        || name
            .strip_prefix('[')
            .and_then(|n| n.strip_suffix(']'))
            .is_some_and(|n| n.parse::<IpAddr>().is_ok())
}

fn write_response(stream: &mut TcpStream, status: u16, body: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let allow = if status == 405 { "Allow: GET\r\n" } else { "" };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         {allow}Connection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()
}

/// Pull one endpoint from a peer at `addr` (`host:port`, optionally
/// `http://`-prefixed; port defaults to [`DEFAULT_PORT`]). Returns the HTTP
/// status and the JSON body.
pub fn pull(addr: &str, version: u32, endpoint: &str, query: &str) -> Result<(u16, Value)> {
    let addr = addr.trim_start_matches("http://").trim_end_matches('/');
    let target = if addr
        .rsplit_once(':')
        .is_some_and(|(h, p)| p.parse::<u16>().is_ok() && (!h.contains(':') || h.ends_with(']')))
    {
        addr.to_string()
    } else {
        format!("{addr}:{DEFAULT_PORT}")
    };
    let socket = target
        .to_socket_addrs()
        .with_context(|| format!("resolve {target}"))?
        .next()
        .with_context(|| format!("resolve {target}"))?;
    let mut stream = TcpStream::connect_timeout(&socket, IO_TIMEOUT)
        .with_context(|| format!("connect {socket}"))?;
    stream.set_read_timeout(Some(WRITE_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    // The Host is the resolved IP, which the server's host check accepts.
    write!(
        stream,
        "GET /peer/v{version}/{endpoint}{query} HTTP/1.1\r\nHost: {socket}\r\n\
         Accept: application/json\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;
    let mut raw = Vec::new();
    stream
        .take(MAX_RESPONSE as u64 + 1)
        .read_to_end(&mut raw)
        .context("read response")?;
    if raw.len() > MAX_RESPONSE {
        anyhow::bail!("response exceeds {MAX_RESPONSE} bytes");
    }
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("malformed response")?;
    let head = std::str::from_utf8(&raw[..split]).context("malformed response")?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .context("malformed status line")?;
    let body = serde_json::from_slice(&raw[split + 4..]).context("response is not JSON")?;
    Ok((status, body))
}
