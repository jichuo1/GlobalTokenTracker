//! Viewer side: connect to a peer (TLS with a pinned certificate, or
//! plaintext to loopback only) and pull one endpoint.

use super::devices::Registry;
use super::keys::PinVerifier;
use super::net::{IO_TIMEOUT, Stream, Wire};
use super::server::DEFAULT_PORT;
use crate::store::Store;
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use serde_json::Value;
use std::io::Write;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

const READ_DEADLINE: Duration = Duration::from_secs(60);
/// Cap on a response body.
const MAX_RESPONSE: usize = 256 << 20;

/// `host[:port]` (optionally `http(s)://`-prefixed) → one socket address.
/// Hosts may be names, IPv4, or IPv6 — bare (`fe80::1%en0`) or bracketed
/// (`[fe80::1%en0]:4319`), link-local with a scope id (interface name or
/// index). The port defaults to [`DEFAULT_PORT`].
pub(crate) fn resolve(addr: &str) -> Result<SocketAddr, &'static str> {
    let addr = addr
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let (host, port) = if let Some(rest) = addr.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or("connect_failed")?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse::<u16>().map_err(|_| "connect_failed")?,
            None if tail.is_empty() => DEFAULT_PORT,
            None => return Err("connect_failed"),
        };
        (host, port)
    } else if addr.matches(':').count() >= 2 {
        // A bare IPv6 address cannot carry a port.
        (addr, DEFAULT_PORT)
    } else if let Some((host, port)) = addr.rsplit_once(':') {
        (host, port.parse::<u16>().map_err(|_| "connect_failed")?)
    } else {
        (addr, DEFAULT_PORT)
    };
    if host.is_empty() {
        return Err("connect_failed");
    }
    (host, port)
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

/// Where and how to reach one device.
#[derive(Clone, Debug)]
pub(crate) struct Target {
    pub candidates: Vec<SocketAddr>,
    /// Pinned certificate fingerprint (`None`: unpaired, `hello` only).
    pub pin: Option<String>,
    pub token: Option<String>,
    pub plaintext: bool,
}

/// One response, and which candidate address answered it.
#[derive(Debug)]
pub(crate) struct Fetched {
    pub addr: SocketAddr,
    pub status: u16,
    pub body: Value,
}

/// GET one endpoint, trying each candidate in order. Every TLS candidate is
/// pin-checked before anything (the token included) is sent; a fingerprint
/// mismatch anywhere is the error reported when no candidate works.
pub(crate) fn fetch(
    target: &Target,
    version: u32,
    endpoint: &str,
    query: &str,
) -> Result<Fetched, &'static str> {
    let mut failure = "connect_failed";
    let mut connected = None;
    for &addr in &target.candidates {
        let verifier = PinVerifier::new(target.pin.as_deref());
        match connect(addr, (!target.plaintext).then_some(&verifier)) {
            Ok(wire) => {
                connected = Some((addr, wire, verifier));
                break;
            }
            Err(code) if failure != "peer_fingerprint_mismatch" => failure = code,
            Err(_) => {}
        }
    }
    let (addr, mut wire, verifier) = connected.ok_or(failure)?;
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    let auth = target
        .token
        .as_ref()
        .filter(|_| endpoint != "hello")
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "GET /peer/v{version}/{endpoint}{query} HTTP/1.1\r\nHost: {addr}\r\n{auth}\
         Accept: application/json\r\nConnection: close\r\n\r\n"
    );
    let (status, body) = exchange(&mut wire, &request)?;
    // Over TLS the identity a server claims must be the certificate it used.
    if status == 200
        && !target.plaintext
        && endpoint == "hello"
        && body["fingerprint"].as_str() != verifier.seen().as_deref()
    {
        return Err("peer_fingerprint_mismatch");
    }
    Ok(Fetched { addr, status, body })
}

/// Pull `endpoint` from `target` — a paired device (fingerprint, device id,
/// name or last address) or a bare address. Paired devices are reached over
/// TLS with their pinned fingerprint and bearer token; an unpaired address
/// allows `hello` only. `plaintext` is honoured for loopback addresses only.
///
/// With `rediscover`, a paired device named by id/name is first looked up on
/// mDNS by device id (its address may have changed), then tried at its last
/// known address. Every candidate is pin-verified; a newly working address
/// is saved.
pub fn pull(
    store: &Store,
    target: &str,
    plaintext: bool,
    rediscover: bool,
    version: u32,
    endpoint: &str,
    query: &str,
) -> anyhow::Result<PullOutcome> {
    let locate = |device_id: &str, fingerprint: &str| {
        if rediscover && !plaintext {
            super::discovery::locate(device_id, fingerprint, super::discovery::LOCATE_TIMEOUT)
        } else {
            Vec::new()
        }
    };
    pull_via(store, target, plaintext, &locate, version, endpoint, query)
}

/// Candidate addresses for a paired device: freshly located first, then the
/// last address that worked.
pub(crate) fn candidates_for(located: &[SocketAddr], last_address: &str) -> Vec<SocketAddr> {
    super::discovery::candidates(located, last_address)
        .iter()
        .filter_map(|c| resolve(c).ok())
        .collect()
}

/// [`pull`] with the mDNS lookup supplied by the caller (tests).
pub(crate) fn pull_via(
    store: &Store,
    target: &str,
    plaintext: bool,
    locate: &dyn Fn(&str, &str) -> Vec<SocketAddr>,
    version: u32,
    endpoint: &str,
    query: &str,
) -> anyhow::Result<PullOutcome> {
    let registry = Registry::open(store)?;
    let found = match registry.find(target) {
        Ok(found) => found,
        Err(e) if e.is::<super::trust::Ambiguous>() => return Ok(Err("ambiguous_peer")),
        Err(e) => return Err(e),
    };
    // An explicit address is used as given; a paired device may move.
    let (device, candidates) = if let Some(device) = found {
        let located = locate(&device.device_id, &device.fingerprint);
        let candidates = candidates_for(&located, &device.last_address);
        (Some(device), candidates)
    } else {
        let addr = match resolve(target) {
            Ok(a) => a,
            // Neither a paired device's name or id nor a reachable address
            // (e.g. a device just forgotten): for the authenticated
            // endpoints that is "not paired", whatever the resolver says.
            Err(_) if endpoint != "hello" => return Ok(Err("not_paired")),
            Err(code) => return Ok(Err(code)),
        };
        match registry.find(&addr.to_string()) {
            Ok(device) => (device, vec![addr]),
            Err(_) => return Ok(Err("ambiguous_peer")),
        }
    };
    if endpoint != "hello" && device.is_none() {
        return Ok(Err("not_paired"));
    }
    let token = match &device {
        Some(d) => registry.token(&d.device_id)?,
        None => None,
    };
    if endpoint != "hello" && token.is_none() {
        return Ok(Err("not_paired"));
    }
    let target = Target {
        candidates,
        pin: device.as_ref().map(|d| d.fingerprint.clone()),
        token,
        plaintext,
    };
    let fetched = match fetch(&target, version, endpoint, query) {
        Ok(f) => f,
        Err(code) => return Ok(Err(code)),
    };
    if let (200, Some(d)) = (fetched.status, &device) {
        let address = fetched.addr.to_string();
        if address != d.last_address && !plaintext {
            registry.set_address(&d.device_id, &address)?;
        }
    }
    Ok(Ok((fetched.status, fetched.body)))
}
