//! Viewer side: connect to a peer (TLS with a pinned certificate, or
//! plaintext to loopback only) and pull one endpoint.

use super::keys::{PinVerifier, peer_dir};
use super::net::{IO_TIMEOUT, Stream, Wire};
use super::server::DEFAULT_PORT;
use super::trust::TrustStore;
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use serde_json::Value;
use std::io::Write;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const READ_DEADLINE: Duration = Duration::from_secs(60);
/// Cap on a response body.
const MAX_RESPONSE: usize = 256 << 20;

/// `host[:port]` (optionally `http(s)://`-prefixed) → one socket address.
pub(crate) fn resolve(addr: &str) -> Result<SocketAddr, &'static str> {
    let addr = addr
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let with_port = if addr.parse::<SocketAddr>().is_ok()
        || addr
            .rsplit_once(':')
            .is_some_and(|(h, p)| p.parse::<u16>().is_ok() && !h.contains(':'))
    {
        addr.to_string()
    } else if addr.contains(':') && !addr.starts_with('[') {
        format!("[{addr}]:{DEFAULT_PORT}")
    } else {
        format!("{addr}:{DEFAULT_PORT}")
    };
    with_port
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .ok_or("connect_failed")
}

/// Open a connection. `Some(verifier)` = TLS 1.3 checked by that verifier
/// (the handshake completes here); `None` = plaintext, loopback only.
pub(crate) fn connect(
    target: SocketAddr,
    tls: Option<&Arc<PinVerifier>>,
) -> Result<Wire, &'static str> {
    if tls.is_none() && !target.ip().is_loopback() {
        return Err("plaintext_not_allowed");
    }
    let tcp = TcpStream::connect_timeout(&target, IO_TIMEOUT).map_err(|_| "connect_failed")?;
    let _ = tcp.set_read_timeout(Some(IO_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(IO_TIMEOUT));
    let Some(verifier) = tls else {
        return Ok(Wire::new(Stream::Plain(tcp)));
    };
    let config = verifier.client_config().map_err(|_| "tls_failed")?;
    let conn = ClientConnection::new(config, ServerName::IpAddress(target.ip().into()))
        .map_err(|_| "tls_failed")?;
    let mut stream = StreamOwned::new(conn, tcp);
    while stream.conn.is_handshaking() {
        if stream.conn.complete_io(&mut stream.sock).is_err() {
            return Err(if verifier.mismatched() {
                "peer_fingerprint_mismatch"
            } else {
                "tls_failed"
            });
        }
    }
    if verifier.mismatched() {
        return Err("peer_fingerprint_mismatch");
    }
    Ok(Wire::new(Stream::TlsClient(Box::new(stream))))
}

pub(crate) fn send(wire: &mut Wire, request: &str) -> std::io::Result<()> {
    wire.stream.write_all(request.as_bytes())?;
    wire.stream.flush()
}

/// Send one GET and read `(status, JSON body)`.
fn exchange(wire: &mut Wire, request: &str) -> Result<(u16, Value), &'static str> {
    send(wire, request).map_err(|_| "connect_failed")?;
    let deadline = Instant::now() + READ_DEADLINE;
    let head = wire
        .read_until(b"\r\n\r\n", 64 << 10, deadline)
        .map_err(|_| "malformed_response")?;
    let status = std::str::from_utf8(&head)
        .ok()
        .and_then(|h| h.split(' ').nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or("malformed_response")?;
    let body = wire
        .read_to_end(MAX_RESPONSE, deadline)
        .map_err(|_| "malformed_response")?;
    wire.stream.close(0);
    let body = serde_json::from_slice(&body).map_err(|_| "malformed_response")?;
    Ok((status, body))
}

/// A failed pull: a fixed code for the caller's JSON failure document.
pub type PullOutcome = Result<(u16, Value), &'static str>;

/// Pull `endpoint` from `target` — a paired device (fingerprint, device id,
/// name or saved address) or a bare address. Paired devices are reached over
/// TLS with their pinned fingerprint and bearer token; an unpaired address
/// allows `hello` only. `plaintext` is honoured for loopback addresses only.
pub fn pull(
    db: &Path,
    target: &str,
    plaintext: bool,
    version: u32,
    endpoint: &str,
    query: &str,
) -> anyhow::Result<PullOutcome> {
    let trust = TrustStore::open(&peer_dir(db))?;
    let peer = match trust.find_peer(target) {
        Ok(found) => found,
        Err(e) if e.is::<super::trust::Ambiguous>() => return Ok(Err("ambiguous_peer")),
        Err(e) => return Err(e),
    };
    let address = peer.as_ref().map_or(target, |p| p.address.as_str());
    let addr = match resolve(address) {
        Ok(a) => a,
        Err(code) => return Ok(Err(code)),
    };
    let peer = peer.or(match trust.find_peer(&addr.to_string()) {
        Ok(found) => found,
        Err(_) => return Ok(Err("ambiguous_peer")),
    });
    if endpoint != "hello" && peer.is_none() {
        return Ok(Err("not_paired"));
    }
    let verifier = PinVerifier::new(peer.as_ref().map(|p| p.fingerprint.as_str()));
    let mut wire = match connect(addr, (!plaintext).then_some(&verifier)) {
        Ok(w) => w,
        Err(code) => return Ok(Err(code)),
    };
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    let auth = peer
        .as_ref()
        .filter(|_| endpoint != "hello")
        .map(|p| format!("Authorization: Bearer {}\r\n", p.token))
        .unwrap_or_default();
    let request = format!(
        "GET /peer/v{version}/{endpoint}{query} HTTP/1.1\r\nHost: {addr}\r\n{auth}\
         Accept: application/json\r\nConnection: close\r\n\r\n"
    );
    let mut result = exchange(&mut wire, &request);
    // Over TLS the identity a server claims must be the certificate it used.
    if let (Ok((200, body)), false) = (&result, plaintext)
        && endpoint == "hello"
        && body["fingerprint"].as_str() != verifier.seen().as_deref()
    {
        result = Err("peer_fingerprint_mismatch");
    }
    if let (Ok((200, _)), Some(p)) = (&result, &peer) {
        trust.touch_peer(&p.fingerprint, crate::store::now_ms())?;
    }
    Ok(result)
}
