//! Bonjour/mDNS discovery for `gtt.peer`: `_gtt._tcp` advertisement while a
//! LAN server runs, browsing for devices, and re-finding paired devices whose
//! address changed.
//!
//! Discovery is a hint, never trust: TXT records carry only the protocol
//! version, device id, display name and a fingerprint *prefix* — nothing
//! usage-related — and pairing/pulling still verifies the full pinned
//! certificate fingerprint over TLS. When mDNS is blocked (guest Wi-Fi,
//! VLANs, client isolation) devices are added by IP instead.
//!
//! Uses `mdns-sd`: pure Rust, runs its own daemon thread (no async runtime).

use super::trust::{Peer, TrustStore};
use super::{Identity, PROTOCOL_VERSION, clean_name};
use anyhow::{Context, Result};
use mdns_sd::{IfKind, ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr, SocketAddrV6};
use std::path::Path;
use std::time::{Duration, Instant};

pub const SERVICE_TYPE: &str = "_gtt._tcp.local.";
/// Hex characters of the certificate fingerprint published in TXT.
pub const FP_PREFIX_LEN: usize = 16;
/// How long a pull waits for a paired device to answer on mDNS.
pub const LOCATE_TIMEOUT: Duration = Duration::from_millis(1500);
/// TXT values are at most 255 bytes including `key=`.
const MAX_NAME_BYTES: usize = 200;

/// What a device publishes in its TXT record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Txt {
    pub protocol_version: u32,
    pub device_id: String,
    pub name: String,
    pub fingerprint_prefix: String,
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Txt {
    /// From a server identity; `None` without a certificate fingerprint.
    pub fn for_identity(identity: &Identity) -> Option<Self> {
        let fp = identity.fingerprint.as_deref()?;
        Some(Self {
            protocol_version: PROTOCOL_VERSION,
            device_id: identity.device_id.clone(),
            name: identity.device_name.clone(),
            fingerprint_prefix: fp.get(..FP_PREFIX_LEN)?.to_string(),
        })
    }

    /// TXT key/value pairs: `v`, `id`, `name`, `fp`.
    pub fn encode(&self) -> Vec<(String, String)> {
        let mut name = String::new();
        for c in self.name.chars() {
            if name.len() + c.len_utf8() > MAX_NAME_BYTES {
                break;
            }
            name.push(c);
        }
        vec![
            ("v".into(), self.protocol_version.to_string()),
            ("id".into(), self.device_id.clone()),
            ("name".into(), name),
            ("fp".into(), self.fingerprint_prefix.clone()),
        ]
    }

    /// Parse and validate untrusted TXT values.
    pub fn decode(get: impl Fn(&str) -> Option<String>) -> Result<Self, &'static str> {
        let protocol_version = get("v")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v >= 1)
            .ok_or("txt_invalid_version")?;
        let device_id = get("id")
            .filter(|id| is_hex(id, 32))
            .ok_or("txt_invalid_device_id")?;
        let name = get("name")
            .and_then(|n| clean_name(&n))
            .ok_or("txt_invalid_name")?;
        let fingerprint_prefix = get("fp")
            .filter(|fp| is_hex(fp, FP_PREFIX_LEN))
            .ok_or("txt_invalid_fingerprint")?;
        Ok(Self {
            protocol_version,
            device_id,
            name,
            fingerprint_prefix,
        })
    }
}

/// One resolved mDNS answer, before merging.
#[derive(Clone, Debug, PartialEq)]
pub struct Sighting {
    pub addresses: Vec<SocketAddr>,
    pub txt: Result<Txt, &'static str>,
}

impl Sighting {
    fn from_resolved(service: &ResolvedService) -> Self {
        let port = service.get_port();
        let addresses = service
            .get_addresses()
            .iter()
            .map(|ip| match ip {
                ScopedIp::V6(v6) => {
                    // Link-local addresses need the interface they were seen on.
                    let scope = if v6.addr().segments()[0] & 0xffc0 == 0xfe80 {
                        v6.scope_id().index
                    } else {
                        0
                    };
                    SocketAddr::V6(SocketAddrV6::new(*v6.addr(), port, 0, scope))
                }
                other => SocketAddr::new(other.to_ip_addr(), port),
            })
            .collect();
        let txt = Txt::decode(|k| service.get_property_val_str(k).map(str::to_string));
        Self { addresses, txt }
    }
}

/// A device seen on the network, merged across interfaces and answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Found {
    pub device_id: String,
    pub name: String,
    pub protocol_version: u32,
    /// Socket addresses (IPv4 first, then global IPv6, then link-local
    /// IPv6 with scope id) — usable as `peer pair <address> <code>`.
    pub addresses: Vec<String>,
    pub port: u16,
    pub fingerprint_prefix: String,
    pub paired: bool,
    /// `fingerprint_mismatch`: paired, but the advertised prefix is not the
    /// pinned certificate's (pulls will fail closed).
    /// `duplicate_device_id`: another certificate advertises the same id
    /// (e.g. a ledger copied to a second machine) — listed separately.
    pub warnings: Vec<String>,
}

fn address_rank(a: &SocketAddr) -> (u8, String) {
    let rank = match a.ip() {
        IpAddr::V4(_) => 0,
        IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80 => 2,
        IpAddr::V6(_) => 1,
    };
    (rank, a.to_string())
}

/// Merge raw answers into one entry per device id and certificate (answers
/// arrive once per interface and address family), and compare against
/// paired devices.
/// Returns the devices and the number of answers with invalid TXT records.
pub fn merge(sightings: &[Sighting], peers: &[Peer]) -> (Vec<Found>, usize) {
    let mut by_id: BTreeMap<(String, String), Found> = BTreeMap::new();
    let mut invalid = 0;
    for s in sightings {
        let Ok(txt) = &s.txt else {
            invalid += 1;
            continue;
        };
        let key = (txt.device_id.clone(), txt.fingerprint_prefix.clone());
        let entry = by_id.entry(key).or_insert_with(|| Found {
            device_id: txt.device_id.clone(),
            name: txt.name.clone(),
            protocol_version: txt.protocol_version,
            addresses: Vec::new(),
            port: s.addresses.first().map_or(0, SocketAddr::port),
            fingerprint_prefix: txt.fingerprint_prefix.clone(),
            paired: false,
            warnings: Vec::new(),
        });
        entry
            .addresses
            .extend(s.addresses.iter().map(ToString::to_string));
    }
    let mut out: Vec<Found> = by_id.into_values().collect();
    let ids: Vec<String> = out.iter().map(|f| f.device_id.clone()).collect();
    for found in &mut out {
        if ids.iter().filter(|id| **id == found.device_id).count() > 1 {
            found.warnings.push("duplicate_device_id".into());
        }
        let mut addrs: Vec<SocketAddr> = found
            .addresses
            .iter()
            .filter_map(|a| a.parse().ok())
            .collect();
        addrs.sort_by_key(address_rank);
        addrs.dedup();
        found.addresses = addrs.iter().map(ToString::to_string).collect();
        if peers.iter().any(|p| p.device_id == found.device_id) {
            found.paired = true;
            if !peers.iter().any(|p| {
                p.device_id == found.device_id
                    && p.fingerprint.starts_with(&found.fingerprint_prefix)
            }) {
                found.warnings.push("fingerprint_mismatch".into());
            }
        }
        found.warnings.sort();
        found.warnings.dedup();
    }
    out.sort_by(|a, b| (&a.name, &a.device_id).cmp(&(&b.name, &b.device_id)));
    (out, invalid)
}

/// Browse `_gtt._tcp` for up to `timeout`, or until `done` accepts an answer.
pub fn browse(timeout: Duration, done: impl Fn(&Sighting) -> bool) -> Result<Vec<Sighting>> {
    let daemon = ServiceDaemon::new().context("peer: start mDNS")?;
    let events = daemon.browse(SERVICE_TYPE).context("peer: mDNS browse")?;
    let deadline = Instant::now() + timeout;
    let mut sightings = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match events.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(service)) => {
                let sighting = Sighting::from_resolved(&service);
                let stop = done(&sighting);
                sightings.push(sighting);
                if stop {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    Ok(sightings)
}

/// `peer discover`: devices on the network, merged and checked against the
/// paired devices in `<data dir>/peer`.
pub fn discover(db: &Path, timeout: Duration) -> Result<(Vec<Found>, usize)> {
    let peers = TrustStore::open(&super::keys::peer_dir(db))?.peers()?;
    let sightings = browse(timeout, |_| false)?;
    Ok(merge(&sightings, &peers))
}

/// Current addresses of the device with `device_id` whose advertised
/// fingerprint prefix matches `fingerprint` (the pin), as advertised.
pub fn locate(device_id: &str, fingerprint: &str, timeout: Duration) -> Vec<SocketAddr> {
    let wanted = |s: &Sighting| {
        s.txt.as_ref().is_ok_and(|t| {
            t.device_id == device_id && fingerprint.starts_with(&t.fingerprint_prefix)
        })
    };
    let Ok(sightings) = browse(timeout, wanted) else {
        return Vec::new();
    };
    let mut addrs: Vec<SocketAddr> = sightings
        .iter()
        .filter(|s| wanted(s))
        .flat_map(|s| s.addresses.iter().copied())
        .collect();
    addrs.sort_by_key(address_rank);
    addrs.dedup();
    addrs
}

/// Where to try a paired device: freshly advertised addresses first, then
/// the last address that worked. Every candidate is still pin-verified.
pub fn candidates(located: &[SocketAddr], saved: &str) -> Vec<String> {
    let mut out: Vec<String> = located.iter().map(ToString::to_string).collect();
    if !out.iter().any(|a| a == saved) {
        out.push(saved.to_string());
    }
    out
}

/// A running `_gtt._tcp` advertisement; withdrawn (mDNS goodbye) on
/// [`Advertisement::withdraw`] or drop.
pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertisement {
    /// Advertise `txt` for a server listening on `bind` (a non-loopback
    /// address). An unspecified bind publishes the host's addresses of that
    /// family; a specific one publishes only it.
    pub fn start(txt: &Txt, bind: SocketAddr) -> Result<Self> {
        anyhow::ensure!(
            !bind.ip().is_loopback(),
            "peer: loopback servers are not advertised"
        );
        let daemon = ServiceDaemon::new().context("peer: start mDNS")?;
        let _ = daemon.disable_interface(if bind.is_ipv4() {
            IfKind::IPv6
        } else {
            IfKind::IPv4
        });
        let instance = format!("{} ({})", txt.name, &txt.device_id[..6]);
        let host = format!("gtt-{}.local.", &txt.device_id[..12]);
        let props = txt.encode();
        let info = if bind.ip().is_unspecified() {
            ServiceInfo::new(
                SERVICE_TYPE,
                &instance,
                &host,
                "",
                bind.port(),
                props.as_slice(),
            )?
            .enable_addr_auto()
        } else {
            ServiceInfo::new(
                SERVICE_TYPE,
                &instance,
                &host,
                bind.ip(),
                bind.port(),
                props.as_slice(),
            )?
        };
        let fullname = info.get_fullname().to_string();
        daemon.register(info).context("peer: mDNS register")?;
        Ok(Self { daemon, fullname })
    }

    /// Send the goodbye and stop the daemon (waits up to a second).
    pub fn withdraw(self) {
        drop(self);
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        if let Ok(status) = self.daemon.unregister(&self.fullname) {
            let _ = status.recv_timeout(Duration::from_secs(1));
        }
        if let Ok(status) = self.daemon.shutdown() {
            let _ = status.recv_timeout(Duration::from_secs(1));
        }
    }
}
