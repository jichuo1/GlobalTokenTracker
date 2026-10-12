//! In-process LAN sharing for the app shells — what `peer serve` does, as a
//! handle a GUI can start, inspect and stop: TLS server on its own thread,
//! mDNS advertisement while it runs, the one-time pairing code, and the
//! paired viewers (list / revoke).
//!
//! Starting is the only write path: like `peer serve` it opens the ledger
//! read-write once (device id, optional name) and creates the device key on
//! first use; every request afterwards reads through read-only connections.

use super::discovery::{Advertisement, Txt};
use super::keys::{DeviceKey, peer_dir};
use super::server::{self, ServerState};
use super::trust::{TrustStore, Viewer};
use crate::store::{Store, now_ms};
use anyhow::{Context, Result};
use serde::Serialize;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Where the shells share: every IPv4 interface on the standard port.
pub const LAN_BIND: &str = "0.0.0.0:4319";

/// What a shell shows about a running share.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ShareStatus {
    pub device_id: String,
    pub device_name: String,
    /// Full certificate fingerprint; viewers pin this.
    pub fingerprint: String,
    pub bind: String,
    pub port: u16,
    /// What another device types into "Add device": this machine's primary
    /// LAN address with the port (empty when offline / loopback-only).
    pub addresses: Vec<String>,
    pub advertised: bool,
    /// The pairing code on offer and its remaining seconds.
    pub code: Option<String>,
    pub code_expires_in: Option<u64>,
}

/// A running share. Dropping it stops the server and withdraws the
/// advertisement.
pub struct Sharing {
    state: Arc<ServerState>,
    local: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    advert: Option<Advertisement>,
    peer_dir: PathBuf,
}

impl std::fmt::Debug for Sharing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sharing")
            .field("local", &self.local)
            .finish_non_exhaustive()
    }
}

impl Sharing {
    /// Bind `bind` (TLS always — plaintext is never offered here), start
    /// serving on a thread, advertise on mDNS when `advertise` and the
    /// address is not loopback, and issue a first pairing code.
    pub fn start(db: &Path, bind: &str, name: Option<&str>, advertise: bool) -> Result<Self> {
        let listener = server::bind(bind, true)?;
        let identity = {
            let store = Store::open(db)?;
            super::ensure_identity(&store, name)?
        };
        let dir = peer_dir(db);
        let device = DeviceKey::load_or_create(&dir)?;
        let state = Arc::new(ServerState::new(db.to_path_buf(), identity, Some(&device))?);
        let local = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (state, stop) = (Arc::clone(&state), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("gtt-peer-serve".into())
                .spawn(move || {
                    let _ = server::serve_until(&listener, state, &stop);
                })
                .context("peer: start server thread")?
        };
        let advert = if advertise && !local.ip().is_loopback() {
            Txt::for_identity(&state.identity)
                .and_then(|txt| Advertisement::start(&txt, local).ok())
        } else {
            None
        };
        let sharing = Self {
            state,
            local,
            stop,
            thread: Some(thread),
            advert,
            peer_dir: dir,
        };
        sharing.new_code()?;
        Ok(sharing)
    }

    /// Issue a fresh one-time code (replaces the current one).
    pub fn new_code(&self) -> Result<String> {
        self.state.issue_code()
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    pub fn status(&self) -> ShareStatus {
        let current = self
            .state
            .pairing
            .lock()
            .ok()
            .and_then(|p| p.current(Instant::now()));
        ShareStatus {
            device_id: self.state.identity.device_id.clone(),
            device_name: self.state.identity.device_name.clone(),
            fingerprint: self.state.identity.fingerprint.clone().unwrap_or_default(),
            bind: self.local.to_string(),
            port: self.local.port(),
            addresses: lan_addresses(self.local),
            advertised: self.advert.is_some(),
            code_expires_in: current.as_ref().map(|(_, left)| left.as_secs()),
            code: current.map(|(code, _)| code),
        }
    }

    /// Viewers that have paired with this device (revoked ones included).
    pub fn viewers(&self) -> Result<Vec<Viewer>> {
        TrustStore::open(&self.peer_dir)?.viewers()
    }

    /// Revoke a viewer by id or name — its very next request fails.
    pub fn revoke(&self, viewer: &str) -> Result<Option<Viewer>> {
        TrustStore::open(&self.peer_dir)?.revoke(viewer, now_ms())
    }

    /// Stop serving: withdraw the advertisement, end the accept loop, wait
    /// for it (requests in flight finish on their own threads).
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        drop(self.advert.take());
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept: connect once to the listening port.
        let wake = if self.local.ip().is_unspecified() {
            SocketAddr::new(
                if self.local.is_ipv4() {
                    std::net::Ipv4Addr::LOCALHOST.into()
                } else {
                    std::net::Ipv6Addr::LOCALHOST.into()
                },
                self.local.port(),
            )
        } else {
            self.local
        };
        let _ = TcpStream::connect_timeout(&wake, Duration::from_secs(1));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Sharing {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.shutdown();
        }
    }
}

/// Paired viewers and revocation without a running share (the shells list
/// them even while sharing is off).
pub fn viewers(db: &Path) -> Result<Vec<Viewer>> {
    TrustStore::open(&peer_dir(db))?.viewers()
}

pub fn revoke(db: &Path, viewer: &str) -> Result<Option<Viewer>> {
    TrustStore::open(&peer_dir(db))?.revoke(viewer, now_ms())
}

/// The address to give other devices: the bound IP, or for a wildcard bind
/// the primary interface's (found by "connecting" a UDP socket — no packet
/// is sent).
pub fn lan_addresses(local: SocketAddr) -> Vec<String> {
    let ip = if local.ip().is_unspecified() {
        std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
            .and_then(|s| s.connect(("192.0.2.1", 9)).map(|()| s))
            .and_then(|s| s.local_addr())
            .map(|a| a.ip())
            .ok()
    } else {
        Some(local.ip())
    };
    ip.filter(|ip| !ip.is_unspecified() && !ip.is_loopback())
        .map(|ip| vec![SocketAddr::new(ip, local.port()).to_string()])
        .unwrap_or_default()
}
