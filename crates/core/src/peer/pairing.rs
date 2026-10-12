//! Pairing: a 6-digit one-time code shown on the server, typed on the viewer.
//!
//! The code must not be checkable offline. If either side sent something like
//! `HMAC(code, fingerprints)`, an interceptor holding that value could try
//! all 10⁶ codes in milliseconds and then finish pairing on both sides. So the
//! code is proven one digit at a time with commit/reveal rounds — the scheme
//! of Bluetooth passkey entry — inside the TLS session, bound to the
//! certificate fingerprint each side sees:
//!
//! ```text
//! T  = SHA-256("gtt.peer/pair/v1" ‖ fp ‖ viewer name ‖ Ns₀ ‖ Nv₀)
//! round i (0‥5), digit dᵢ:
//!   V → S  Cv = HMAC(Nvᵢ, "V" ‖ T ‖ i ‖ dᵢ)      viewer commits
//!   S → V  Cs = HMAC(Nsᵢ, "S" ‖ T ‖ i ‖ dᵢ)      server commits
//!   V → S  Nvᵢ          server checks Cv against its own dᵢ, else fails
//!   S → V  Nsᵢ          viewer checks Cs against its own dᵢ, else fails
//! S → V  bearer token (after all six rounds)
//! ```
//!
//! Each side commits before it learns the other side's digit and reveals only
//! after the other side has committed, so an active interceptor must guess
//! every digit it relays — 1 in 10⁶ — and any wrong guess aborts the run.
//! The "V"/"S" role labels stop commitments being reflected back; `T` binds
//! the run to the certificate the viewer actually sees, so even a perfect
//! relay through another certificate fails. The server burns the code after
//! one success or five failures and backs off between failures; the viewer
//! refuses to retry a code that failed (a failure may mean interception).
//!
//! Pairing is mutual. A server that supports it says so in the grant
//! (`"reciprocal": true`); the viewer then offers access to itself in the
//! same TLS session — its device id, name, certificate fingerprint, sharing
//! port and a fresh bearer token for its own server — and the server saves it
//! as a paired device (pinned to that fingerprint, at the connection's source
//! address) and asks for a sync. The offer is as trustworthy as the pairing:
//! only the side that proved the code reaches this step, and every later
//! connection is still pin-checked. Older peers skip the step (an old server
//! never sets the flag; an old viewer just closes), so pairing stays one-way
//! with them.

use super::devices::{PeerDevice, Registry};
use super::keys::{DeviceKey, hex, peer_dir};
use super::net::{Wire, WireError};
use anyhow::Result;
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant};

pub const CODE_DIGITS: usize = 6;
pub const CODE_TTL: Duration = Duration::from_secs(120);
/// Failed attempts that burn a code (a new one must be issued).
pub const MAX_FAILURES: u32 = 5;
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Whole exchange, after the TLS handshake.
pub(crate) const PAIR_DEADLINE: Duration = Duration::from_secs(30);
/// The reciprocal step after the grant.
const RECIPROCAL_DEADLINE: Duration = Duration::from_secs(10);
pub(crate) const UPGRADE: &str = "gtt-pair/1";
const LABEL: &[u8] = b"gtt.peer/pair/v1";

pub(crate) fn random<const N: usize>() -> Result<[u8; N]> {
    let mut out = [0u8; N];
    SystemRandom::new()
        .fill(&mut out)
        .map_err(|_| anyhow::anyhow!("peer: system randomness unavailable"))?;
    Ok(out)
}

/// A fresh random bearer token (256 bits, hex).
pub(crate) fn new_token() -> Result<String> {
    Ok(hex(&random::<32>()?))
}

/// Uniform 6-digit code.
fn new_code() -> Result<String> {
    loop {
        let n = u32::from_le_bytes(random::<4>()?);
        // Reject the top sliver so `% 10⁶` stays uniform.
        if n < 4_294_000_000 {
            return Ok(format!("{:06}", n % 1_000_000));
        }
    }
}

pub(crate) fn valid_code(code: &str) -> bool {
    code.len() == CODE_DIGITS && code.bytes().all(|b| b.is_ascii_digit())
}

/// Server-side code lifecycle: issue, expire, single use, back-off, burn.
#[derive(Debug, Default)]
pub struct Pairing {
    code: Option<(String, Instant)>,
    failures: u32,
    locked_until: Option<Instant>,
    busy: bool,
}

impl Pairing {
    /// Issue a new code (replacing any previous one, clearing back-off).
    pub fn issue(&mut self, now: Instant) -> Result<String> {
        let code = new_code()?;
        self.code = Some((code.clone(), now + CODE_TTL));
        self.failures = 0;
        self.locked_until = None;
        Ok(code)
    }

    /// The code on offer and how long it stays valid (`None` once used,
    /// burned or expired) — for a shell that shows it.
    pub fn current(&self, now: Instant) -> Option<(String, Duration)> {
        let (code, expires) = self.code.as_ref()?;
        let left = expires.saturating_duration_since(now);
        (!left.is_zero()).then(|| (code.clone(), left))
    }

    /// Still backing off after a failure.
    pub fn locked(&self, now: Instant) -> bool {
        self.locked_until.is_some_and(|t| now < t)
    }

    /// Claim the code for one pairing run.
    pub(crate) fn begin(&mut self, now: Instant) -> Result<String, &'static str> {
        if self.busy {
            return Err("pairing_busy");
        }
        if self.locked(now) {
            return Err("pairing_locked");
        }
        match &self.code {
            None => Err("pairing_unavailable"),
            Some((_, expires)) if now >= *expires => {
                self.code = None;
                Err("pairing_code_expired")
            }
            Some((code, _)) => {
                self.busy = true;
                Ok(code.clone())
            }
        }
    }

    /// End a run. Success consumes the code; a failure (including an
    /// abandoned run) backs off 1, 2, 4 … s and burns the code at the
    /// fifth.
    pub(crate) fn finish(&mut self, success: bool, now: Instant) {
        self.busy = false;
        if success {
            self.code = None;
            self.failures = 0;
            self.locked_until = None;
            return;
        }
        self.failures += 1;
        let backoff = Duration::from_secs(1 << (self.failures - 1).min(5)).min(MAX_BACKOFF);
        self.locked_until = Some(now + backoff);
        if self.failures >= MAX_FAILURES {
            self.code = None;
        }
    }
}

fn transcript(fingerprint: &str, name: &str, ns0: &[u8], nv0: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    for part in [LABEL, fingerprint.as_bytes(), name.as_bytes(), ns0, nv0] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part);
    }
    h.finalize().into()
}

fn commitment(nonce: &[u8], role: u8, t: &[u8; 32], round: usize, digit: u8) -> Vec<u8> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, nonce);
    let mut ctx = hmac::Context::with_key(&key);
    ctx.update(LABEL);
    ctx.update(&[role]);
    ctx.update(t);
    ctx.update(&[round as u8, digit]);
    ctx.sign().as_ref().to_vec()
}

fn opens(nonce: &[u8], role: u8, t: &[u8; 32], round: usize, digit: u8, commit: &[u8]) -> bool {
    let key = hmac::Key::new(hmac::HMAC_SHA256, nonce);
    let mut msg = Vec::with_capacity(64);
    msg.extend_from_slice(LABEL);
    msg.push(role);
    msg.extend_from_slice(t);
    msg.extend_from_slice(&[round as u8, digit]);
    hmac::verify(&key, &msg, commit).is_ok()
}

fn unhex(v: &Value, field: &str, len: usize) -> Option<Vec<u8>> {
    let s = v.get(field)?.as_str()?;
    if s.len() != len * 2 {
        return None;
    }
    (0..len)
        .map(|i| u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect()
}

fn digits(code: &str) -> Vec<u8> {
    code.bytes().map(|b| b - b'0').collect()
}

/// Why a run ended without pairing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RunError {
    /// The other side's proof did not match (wrong code or interception).
    Mismatch,
    /// The other side reported a fixed error code.
    Remote(String),
    /// I/O, timeout or a malformed message.
    Broken,
}

impl From<WireError> for RunError {
    fn from(_: WireError) -> Self {
        Self::Broken
    }
}

impl From<std::io::Error> for RunError {
    fn from(_: std::io::Error) -> Self {
        Self::Broken
    }
}

impl From<anyhow::Error> for RunError {
    fn from(_: anyhow::Error) -> Self {
        Self::Broken
    }
}

fn remote_error(msg: &Value) -> Option<RunError> {
    msg.get("error")
        .and_then(Value::as_str)
        .map(|c| RunError::Remote(c.into()))
}

/// Server half, after the `101 Switching Protocols`. Returns the issued
/// token on success; the caller registers the viewer only then.
pub(crate) fn run_server(
    wire: &mut Wire,
    code: &str,
    fingerprint: &str,
    viewer_name: &str,
    issue: impl FnOnce() -> Result<Value>,
) -> Result<(), RunError> {
    let deadline = Instant::now() + PAIR_DEADLINE;
    let ns0 = random::<32>()?;
    wire.send_json(&json!({"ns0": hex(&ns0)}))?;
    let hello = wire.recv_json(deadline)?;
    let nv0 = unhex(&hello, "nv0", 32).ok_or(RunError::Broken)?;
    let t = transcript(fingerprint, viewer_name, &ns0, &nv0);
    for (round, digit) in digits(code).into_iter().enumerate() {
        let msg = wire.recv_json(deadline)?;
        let cv = unhex(&msg, "c", 32).ok_or(RunError::Broken)?;
        let ns = random::<32>()?;
        wire.send_json(&json!({"c": hex(&commitment(&ns, b'S', &t, round, digit))}))?;
        let msg = wire.recv_json(deadline)?;
        let nv = unhex(&msg, "n", 32).ok_or(RunError::Broken)?;
        if !opens(&nv, b'V', &t, round, digit, &cv) {
            let _ = wire.send_json(&json!({"error": "pairing_failed"}));
            return Err(RunError::Mismatch);
        }
        wire.send_json(&json!({"n": hex(&ns)}))?;
    }
    let grant = issue()?;
    wire.send_json(&grant)?;
    Ok(())
}

/// Viewer half. Returns the server's final grant message.
pub(crate) fn run_viewer(
    wire: &mut Wire,
    code: &str,
    fingerprint: &str,
    viewer_name: &str,
) -> Result<Value, RunError> {
    let deadline = Instant::now() + PAIR_DEADLINE;
    let hello = wire.recv_json(deadline)?;
    let ns0 = unhex(&hello, "ns0", 32).ok_or(RunError::Broken)?;
    let nv0 = random::<32>()?;
    wire.send_json(&json!({"nv0": hex(&nv0)}))?;
    let t = transcript(fingerprint, viewer_name, &ns0, &nv0);
    for (round, digit) in digits(code).into_iter().enumerate() {
        let nv = random::<32>()?;
        wire.send_json(&json!({"c": hex(&commitment(&nv, b'V', &t, round, digit))}))?;
        let msg = wire.recv_json(deadline)?;
        let cs =
            unhex(&msg, "c", 32).ok_or_else(|| remote_error(&msg).unwrap_or(RunError::Broken))?;
        wire.send_json(&json!({"n": hex(&nv)}))?;
        let msg = wire.recv_json(deadline)?;
        let ns =
            unhex(&msg, "n", 32).ok_or_else(|| remote_error(&msg).unwrap_or(RunError::Broken))?;
        if !opens(&ns, b'S', &t, round, digit, &cs) {
            return Err(RunError::Mismatch);
        }
    }
    let grant = wire.recv_json(deadline)?;
    if let Some(e) = remote_error(&grant) {
        return Err(e);
    }
    Ok(grant)
}

/// Result of [`pair`]: the saved device, or a fixed error code.
pub type PairOutcome = std::result::Result<PeerDevice, &'static str>;

/// A successful [`pair_mutual`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paired {
    pub device: PeerDevice,
    /// The other device can read this one too (it accepted the reciprocal
    /// offer). The caller should be sharing, or start now, so it can.
    pub mutual: bool,
}

/// What this device offers back: who it is and where its server listens.
#[derive(Clone, Debug)]
struct Offer {
    device_id: String,
    device_name: String,
    fingerprint: String,
    port: u16,
}

impl Offer {
    /// This device's sharing identity. Creates the device key on first use,
    /// so the offer is valid before sharing has ever run.
    fn local(store: &crate::store::Store) -> Result<Self> {
        let identity = super::ensure_identity(store, None)?;
        let path = store
            .path()
            .ok_or_else(|| anyhow::anyhow!("peer: an in-memory ledger cannot share"))?;
        let key = DeviceKey::load_or_create(&peer_dir(path))?;
        Ok(Self {
            device_id: identity.device_id,
            device_name: identity.device_name,
            fingerprint: key.fingerprint,
            port: super::share::LAN_PORT,
        })
    }
}

/// Viewer: pair with the device at `addr` using `code`; on success register
/// it in the ledger's `peers` (pinned fingerprint) and keep the bearer token
/// in `<data dir>/peer/peer.db`. One-way (see [`pair_mutual`]).
pub fn pair(
    store: &crate::store::Store,
    addr: &str,
    code: &str,
    viewer_name: &str,
) -> Result<PairOutcome> {
    Ok(pair_mutual(store, addr, code, viewer_name, false)?.map(|p| p.device))
}

/// [`pair`], and with `mutual` also offer the other device access to this
/// one (see the module docs).
pub fn pair_mutual(
    store: &crate::store::Store,
    addr: &str,
    code: &str,
    viewer_name: &str,
    mutual: bool,
) -> Result<std::result::Result<Paired, &'static str>> {
    if !valid_code(code) {
        return Ok(Err("invalid_code"));
    }
    let offer = if mutual {
        Some(Offer::local(store)?)
    } else {
        None
    };
    let registry = Registry::open(store)?;
    let trust = &registry.trust;
    let now = crate::store::now_ms();
    if trust.code_failed_recently(code, now)? {
        return Ok(Err("pairing_code_reused"));
    }
    let target = match super::client::resolve(addr) {
        Ok(t) => t,
        Err(code) => return Ok(Err(code)),
    };
    let verifier = super::keys::PinVerifier::new(None);
    let mut wire = match super::client::connect(target, Some(&verifier)) {
        Ok(w) => w,
        Err(code) => return Ok(Err(code)),
    };
    let Some(fingerprint) = verifier.seen() else {
        return Ok(Err("tls_failed"));
    };
    let name = super::clean_name(viewer_name).unwrap_or_else(|| "viewer".into());
    let request = format!(
        "GET /peer/v{}/pair?name={} HTTP/1.1\r\nHost: {target}\r\nConnection: Upgrade\r\nUpgrade: {UPGRADE}\r\n\r\n",
        super::PROTOCOL_VERSION,
        super::percent_encode(&name),
    );
    if super::client::send(&mut wire, &request).is_err() {
        return Ok(Err("connect_failed"));
    }
    let head = match wire.read_until(b"\r\n\r\n", 8192, Instant::now() + PAIR_DEADLINE) {
        Ok(h) => String::from_utf8_lossy(&h).into_owned(),
        Err(_) => return Ok(Err("connect_failed")),
    };
    if !head.starts_with("HTTP/1.1 101 ") {
        // A refusal before the exchange starts (busy, locked, expired …)
        // carries the usual JSON failure body.
        let body = wire
            .read_to_end(64 << 10, Instant::now() + PAIR_DEADLINE)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let code = body
            .as_ref()
            .and_then(|b| b["error"]["code"].as_str())
            .and_then(super::known_code)
            .unwrap_or("pairing_refused");
        return Ok(Err(code));
    }
    let grant = match run_viewer(&mut wire, code, &fingerprint, &name) {
        Ok(grant) => grant,
        Err(error) => {
            wire.stream.close(0);
            trust.note_failed_code(code, now)?;
            return Ok(Err(match error {
                RunError::Mismatch => "pairing_failed",
                RunError::Remote(c) => super::known_code(&c).unwrap_or("pairing_failed"),
                RunError::Broken => "pairing_aborted",
            }));
        }
    };
    let (Some(token), Some(device_id), Some(device_name)) = (
        grant["token"].as_str().filter(|t| t.len() == 64),
        grant["device_id"]
            .as_str()
            .filter(|id| super::valid_device_id(id)),
        grant["device_name"].as_str(),
    ) else {
        wire.stream.close(0);
        trust.note_failed_code(code, now)?;
        return Ok(Err("pairing_aborted"));
    };
    if device_id == super::ensure_device_id(store)? {
        wire.stream.close(0);
        return Ok(Err("pairing_self"));
    }
    let device = PeerDevice {
        device_id: device_id.into(),
        name: super::clean_name(device_name).unwrap_or_else(|| "GTT".into()),
        fingerprint,
        last_address: target.to_string(),
        paired_at: now,
        last_seen_at: Some(now),
        last_pull_at: None,
        last_error_code: None,
        gtt_version: None,
        protocol_version: Some(super::PROTOCOL_VERSION),
        enabled: true,
    };
    registry.save_paired(&device, token)?;
    // The pairing stands either way; the reciprocal step only adds the
    // other direction.
    let mutual = if grant["reciprocal"].as_bool() == Some(true) {
        match &offer {
            Some(offer) => {
                offer_reciprocal(&mut wire, &registry, offer, &device.name, now).unwrap_or(false)
            }
            None => {
                let _ = wire.send_json(&json!({"reciprocal": null}));
                false
            }
        }
    } else {
        false
    };
    wire.stream.close(0);
    Ok(Ok(Paired { device, mutual }))
}

/// Viewer side of the reciprocal step: register a token for the other
/// device on this device's server, offer it, and keep it only if accepted.
fn offer_reciprocal(
    wire: &mut super::net::Wire,
    registry: &Registry<'_>,
    offer: &Offer,
    server_name: &str,
    now: i64,
) -> Result<bool> {
    let token = new_token()?;
    let viewer = registry.trust.add_viewer(server_name, &token, now)?;
    let sent = wire.send_json(&json!({"reciprocal": {
        "device_id": offer.device_id,
        "device_name": offer.device_name,
        "fingerprint": offer.fingerprint,
        "port": offer.port,
        "token": token,
    }}));
    let accepted = sent.is_ok()
        && wire
            .recv_json(Instant::now() + RECIPROCAL_DEADLINE)
            .is_ok_and(|ack| ack["reciprocal"] == "accepted");
    if !accepted {
        registry.trust.revoke(&viewer.id, now)?;
    }
    Ok(accepted)
}

/// Server side of the reciprocal step, after the grant: read the viewer's
/// offer (an old viewer just closes) and save it as a paired device reached
/// at `peer_ip` on the offered port, then ask for a sync. `Ok(true)` when
/// the viewer is now paired in this direction too.
pub(crate) fn accept_reciprocal(
    wire: &mut super::net::Wire,
    db: &Path,
    own_device_id: &str,
    peer_ip: IpAddr,
) -> Result<bool> {
    let Ok(msg) = wire.recv_json(Instant::now() + RECIPROCAL_DEADLINE) else {
        return Ok(false);
    };
    let Some(offer) = msg.get("reciprocal").filter(|o| o.is_object()) else {
        return Ok(false);
    };
    let field = |k: &str| offer.get(k).and_then(Value::as_str);
    let (Some(device_id), Some(name), Some(fingerprint), Some(token), Some(port)) = (
        field("device_id").filter(|id| super::valid_device_id(id) && *id != own_device_id),
        field("device_name").and_then(super::clean_name),
        field("fingerprint").filter(|fp| {
            fp.len() == 64 && fp.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        }),
        field("token").filter(|t| t.len() == 64),
        offer
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p != 0),
    ) else {
        let _ = wire.send_json(&json!({"error": "pairing_aborted"}));
        return Ok(false);
    };
    let store = crate::store::Store::open(db)?;
    let now = crate::store::now_ms();
    let device = PeerDevice {
        device_id: device_id.into(),
        name,
        fingerprint: fingerprint.into(),
        last_address: SocketAddr::new(peer_ip, port).to_string(),
        paired_at: now,
        last_seen_at: Some(now),
        last_pull_at: None,
        last_error_code: None,
        gtt_version: None,
        protocol_version: Some(super::PROTOCOL_VERSION),
        enabled: true,
    };
    Registry::open(&store)?.save_paired(&device, token)?;
    super::sync::request_sync(&store)?;
    wire.send_json(&json!({"reciprocal": "accepted"}))?;
    Ok(true)
}

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) fn wrong_code(code: &str) -> String {
        let mut d: Vec<u8> = code.bytes().collect();
        d[5] = if d[5] == b'9' { b'0' } else { d[5] + 1 };
        String::from_utf8(d).unwrap()
    }
}
