//! Minimal HTTP/1.1 server for `gtt.peer` over `std::net` + rustls — no
//! tokio/axum, the same approach as the OTLP receiver.
//!
//! GET only, one request per connection (`Connection: close`), one thread per
//! connection up to [`MAX_CONNECTIONS`], hard limits on the request head and
//! on how long a client may take to send it. Routes are
//! `/peer/v{N}/{hello|days|summary|quotas|sources|pair}`. Everything except
//! `hello` and `pair` needs `Authorization: Bearer <token>` from pairing.
//! TLS 1.3 is mandatory unless the listener is loopback-only. The `Host`
//! header must be an IP literal or `localhost`, so a web page cannot reach a
//! loopback server through DNS rebinding.

use super::keys::{DeviceKey, peer_dir};
use super::net::{IO_TIMEOUT, Stream, Wire, WireError};
use super::pairing::{self, Pairing, RunError};
use super::trust::TrustStore;
use super::{Identity, Request, SummaryQuery, failure, respond};
use anyhow::{Context, Result};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::{Value, json};
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 4319;
/// Loopback only unless the user explicitly binds elsewhere.
pub const DEFAULT_BIND: &str = "127.0.0.1:4319";
/// Request line + headers (a summary's `known` list rides in the URL).
pub const MAX_HEAD: usize = 64 << 10;
pub const MAX_CONNECTIONS: usize = 16;
/// Total time a client gets to deliver its request head (TLS handshake included).
pub const HEAD_DEADLINE: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything a running server needs.
pub struct ServerState {
    pub db: PathBuf,
    pub identity: Identity,
    pub peer_dir: PathBuf,
    tls: Option<Arc<ServerConfig>>,
    pub pairing: Mutex<Pairing>,
}

impl ServerState {
    /// `device`: `Some` serves TLS with that key; `None` serves plaintext.
    pub fn new(db: PathBuf, mut identity: Identity, device: Option<&DeviceKey>) -> Result<Self> {
        let tls = device.map(DeviceKey::server_config).transpose()?;
        if let Some(d) = device {
            identity.fingerprint = Some(d.fingerprint.clone());
        }
        Ok(Self {
            peer_dir: peer_dir(&db),
            db,
            identity,
            tls,
            pairing: Mutex::new(Pairing::default()),
        })
    }

    pub fn tls(&self) -> bool {
        self.tls.is_some()
    }

    /// Issue a fresh one-time pairing code (TLS servers only).
    pub fn issue_code(&self) -> Result<String> {
        anyhow::ensure!(self.tls(), "pairing requires TLS");
        self.pairing
            .lock()
            .map_err(|_| anyhow::anyhow!("pairing state poisoned"))?
            .issue(Instant::now())
    }
}

/// Bind a listener, refusing non-loopback addresses without TLS. (Pairing
/// is always available on TLS listeners.)
pub fn bind(addr: &str, tls: bool) -> Result<TcpListener> {
    let addrs: Vec<SocketAddr> = std::net::ToSocketAddrs::to_socket_addrs(addr)
        .with_context(|| format!("peer: invalid bind address {addr}"))?
        .collect();
    if !tls && addrs.iter().any(|a| !a.ip().is_loopback()) {
        anyhow::bail!("peer: plaintext is only allowed on loopback; {addr} requires TLS");
    }
    TcpListener::bind(addrs.as_slice()).with_context(|| format!("peer: bind {addr}"))
}

/// Serve forever on `listener` (blocking). Each accepted connection gets its
/// own thread; beyond [`MAX_CONNECTIONS`] concurrent ones the server answers
/// `503 server_busy` and closes.
pub fn serve(listener: &TcpListener, state: Arc<ServerState>) -> Result<()> {
    serve_until(listener, state, &AtomicBool::new(false))
}

/// [`serve`] until `stop` is set; the setter then wakes the accept loop by
/// connecting once (see `share::Sharing::stop`).
pub fn serve_until(
    listener: &TcpListener,
    state: Arc<ServerState>,
    stop: &AtomicBool,
) -> Result<()> {
    if !state.tls() && !listener.local_addr()?.ip().is_loopback() {
        anyhow::bail!("peer: plaintext is only allowed on loopback");
    }
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else { continue };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            busy(stream, &state);
            continue;
        }
        let state = Arc::clone(&state);
        let worker_active = Arc::clone(&active);
        let spawned = std::thread::Builder::new()
            .name("gtt-peer-conn".into())
            .spawn(move || {
                let _ = handle(stream, &state);
                worker_active.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::SeqCst);
        }
    }
    Ok(())
}

/// Over capacity: plaintext gets a 503 document; a TLS client just sees the
/// connection close (a handshake would cost the thread we don't have).
fn busy(mut stream: TcpStream, state: &ServerState) {
    if !state.tls() {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
        let _ = write_response(&mut stream, 503, &failure("request", "server_busy"), &[]);
    }
}

fn handle(tcp: TcpStream, state: &ServerState) -> Result<()> {
    tcp.set_read_timeout(Some(IO_TIMEOUT))?;
    tcp.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let stream = match &state.tls {
        Some(config) => Stream::TlsServer(Box::new(StreamOwned::new(
            ServerConnection::new(Arc::clone(config))?,
            tcp,
        ))),
        None => Stream::Plain(tcp),
    };
    let mut wire = Wire::new(stream);
    let head = wire.read_until(b"\r\n\r\n", MAX_HEAD, Instant::now() + HEAD_DEADLINE);
    let (status, body, extra) = match head {
        Ok(head) => match String::from_utf8(head) {
            Ok(head) => match route(&head, state) {
                Route::Respond(status, body, extra) => (status, body, extra),
                Route::Pair(name) => {
                    pair(&mut wire, state, &name);
                    wire.stream.close(0);
                    return Ok(());
                }
            },
            Err(_) => (400, failure("request", "invalid_request"), &[][..]),
        },
        Err(WireError::TooLarge) => (431, failure("request", "request_too_large"), &[][..]),
        Err(WireError::Timeout) => (408, failure("request", "request_timeout"), &[][..]),
        Err(WireError::Closed) => return Ok(()),
    };
    write_response(&mut wire.stream, status, &body, extra)?;
    wire.stream.close(MAX_HEAD as u64 * 4);
    Ok(())
}

enum Route {
    Respond(u16, Value, &'static [&'static str]),
    Pair(String),
}

const WWW_AUTH: &[&str] = &["WWW-Authenticate: Bearer"];
const ALLOW_GET: &[&str] = &["Allow: GET"];

/// Map a request head to a response (or the pairing upgrade).
fn route(head: &str, state: &ServerState) -> Route {
    let fail = |status, op, code| Route::Respond(status, failure(op, code), &[]);
    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split(' ');
    let (method, target, http) = (
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
    );
    if !http.starts_with("HTTP/1.") || parts.next().is_some() {
        return fail(400, "request", "invalid_request");
    }
    if method != "GET" {
        return Route::Respond(405, failure("request", "method_not_allowed"), ALLOW_GET);
    }
    let (mut host_ok, mut bearer, mut upgrade) = (false, None, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return fail(400, "request", "invalid_request");
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") {
            host_ok = allowed_host(value);
        } else if name.eq_ignore_ascii_case("authorization") {
            bearer = Some(
                value
                    .strip_prefix("Bearer ")
                    .map(str::trim)
                    .unwrap_or_default(),
            );
        } else if name.eq_ignore_ascii_case("upgrade") {
            upgrade = Some(value);
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value != "0")
        {
            return fail(413, "request", "request_too_large");
        }
    }
    if !host_ok {
        return fail(403, "request", "host_not_allowed");
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let Some((version, endpoint)) = path
        .strip_prefix("/peer/v")
        .and_then(|rest| rest.split_once('/'))
    else {
        return fail(404, "request", "unknown_endpoint");
    };
    let Ok(version) = version.parse::<u32>() else {
        return fail(404, "request", "unknown_endpoint");
    };
    let request = match endpoint {
        "hello" => Request::Hello,
        "days" => Request::Days,
        "summary" => match SummaryQuery::parse(query) {
            Some(q) => Request::Summary(q),
            None => return fail(400, "summary", "invalid_request"),
        },
        "quotas" => Request::Quotas,
        "sources" => Request::Sources,
        "pair" => {
            if version != super::PROTOCOL_VERSION {
                return fail(400, "pair", "unsupported_protocol_version");
            }
            return pair_route(state, query, upgrade);
        }
        _ => return fail(404, "request", "unknown_endpoint"),
    };
    let op = request.operation();
    if !matches!(request, Request::Summary(_)) && !query.is_empty() {
        return fail(400, op, "invalid_request");
    }
    if version != super::PROTOCOL_VERSION {
        return fail(400, op, "unsupported_protocol_version");
    }
    if request != Request::Hello {
        let Some(token) = bearer else {
            return Route::Respond(401, failure(op, "auth_required"), WWW_AUTH);
        };
        match TrustStore::open(&state.peer_dir)
            .and_then(|t| t.authorize(token, crate::store::now_ms()))
        {
            Ok(Some(_)) => {}
            Ok(None) => return Route::Respond(401, failure(op, "auth_invalid"), WWW_AUTH),
            Err(_) => return fail(503, op, "auth_unavailable"),
        }
    }
    // Ledger-level outcomes (busy, missing, disabled …) are 200 with
    // `status: unavailable` and a fixed `error.code`, like coordination.
    Route::Respond(
        200,
        respond(&state.db, &state.identity, version, &request),
        &[],
    )
}

fn pair_route(state: &ServerState, query: &str, upgrade: Option<&str>) -> Route {
    let fail = |status, code| Route::Respond(status, failure("pair", code), &[]);
    if !state.tls() {
        return fail(403, "tls_required");
    }
    if upgrade != Some(pairing::UPGRADE) {
        return fail(400, "invalid_request");
    }
    let name = query
        .strip_prefix("name=")
        .and_then(super::percent_decode)
        .and_then(|n| super::clean_name(&n));
    match name {
        Some(name) => Route::Pair(name),
        None => fail(400, "invalid_request"),
    }
}

/// Run the pairing exchange on an upgraded connection.
fn pair(wire: &mut Wire, state: &ServerState, viewer: &str) {
    let fingerprint = state.identity.fingerprint.clone().unwrap_or_default();
    let begun = match state.pairing.lock() {
        Ok(mut p) => p.begin(Instant::now()),
        Err(_) => Err("pairing_unavailable"),
    };
    let code = match begun {
        Ok(code) => code,
        Err(code) => {
            let status = if matches!(code, "pairing_busy" | "pairing_locked") {
                429
            } else {
                403
            };
            let _ = write_response(&mut wire.stream, status, &failure("pair", code), &[]);
            return;
        }
    };
    let upgraded = write!(
        wire.stream,
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: {}\r\n\r\n",
        pairing::UPGRADE
    )
    .and_then(|()| wire.stream.flush());
    let outcome = upgraded.map_err(RunError::from).and_then(|()| {
        pairing::run_server(wire, &code, &fingerprint, viewer, || {
            let token = pairing::new_token()?;
            let added = TrustStore::open(&state.peer_dir)?.add_viewer(
                viewer,
                &token,
                crate::store::now_ms(),
            )?;
            Ok(json!({"token": token, "viewer_id": added.id,
                "device_id": state.identity.device_id,
                "device_name": state.identity.device_name,
                "fingerprint": fingerprint}))
        })
    });
    if let Ok(mut p) = state.pairing.lock() {
        p.finish(outcome.is_ok(), Instant::now());
    }
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

fn write_response(
    stream: &mut impl Write,
    status: u16,
    body: &Value,
    extra: &[&str],
) -> std::io::Result<()> {
    let body = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Content Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let mut extra_headers = String::new();
    for h in extra {
        extra_headers.push_str(h);
        extra_headers.push_str("\r\n");
    }
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         {extra_headers}Connection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()
}
