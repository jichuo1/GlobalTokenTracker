use anyhow::{Context, Result};
use clap::Subcommand;
use globaltokentracker_core::Store;
use globaltokentracker_core::peer::{
    self, SummaryQuery, client, discovery,
    keys::{DeviceKey, peer_dir},
    pairing,
    server::{self, ServerState},
    trust::TrustStore,
};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Subcommand)]
pub enum Command {
    /// Serve this ledger's aggregates read-only to paired viewers (TLS 1.3).
    Serve {
        /// Listen address. Loopback by default; a LAN address shares with
        /// paired viewers over TLS.
        #[arg(long, default_value = server::DEFAULT_BIND)]
        bind: String,
        /// Device display name to save and announce in `hello`.
        #[arg(long)]
        name: Option<String>,
        /// No TLS (loopback addresses only; pairing is then unavailable).
        #[arg(long)]
        plaintext: bool,
        /// Do not advertise `_gtt._tcp` on mDNS (LAN binds advertise by default).
        #[arg(long)]
        no_advertise: bool,
    },
    /// Browse the local network for GTT devices (mDNS `_gtt._tcp`). Results
    /// are hints: pairing and pulls still verify the pinned certificate.
    Discover {
        /// Seconds to listen for answers.
        #[arg(long, default_value_t = 3)]
        timeout: u64,
        #[arg(long, default_value = "json", value_parser = ["json"])]
        format: String,
    },
    /// Pair with a device using the 6-digit code it shows; pins its certificate.
    Pair {
        /// Device address: host[:port], IPv4 or IPv6 (`[fe80::1%en0]:4319` for
        /// link-local); the port defaults to 4319. Use an address from
        /// `peer discover`, or type it when mDNS is blocked.
        addr: String,
        /// The one-time code printed by `peer serve`.
        code: String,
        /// This viewer's name as the server will list it.
        #[arg(long)]
        name: Option<String>,
    },
    /// Fetch one endpoint from a peer and print its JSON document.
    Pull {
        /// A paired device (name, device id, fingerprint or saved address),
        /// or an address (only `hello` works unpaired).
        target: String,
        #[arg(long, default_value = "hello", value_parser = peer::ENDPOINTS)]
        endpoint: String,
        /// Summary only: UTC day lower bound, YYYY-MM-DD.
        #[arg(long)]
        from: Option<String>,
        /// Summary only: fingerprints already held, DAY:FP[,DAY:FP…].
        #[arg(long)]
        known: Option<String>,
        /// Talk to a `--plaintext` server (loopback addresses only).
        #[arg(long)]
        plaintext: bool,
        /// Skip the mDNS lookup of a paired device's current address.
        #[arg(long)]
        no_discover: bool,
        #[arg(long, default_value = "json", value_parser = ["json"])]
        format: String,
        #[arg(long, default_value_t = peer::PROTOCOL_VERSION)]
        protocol_version: u32,
    },
    /// Pull every enabled paired device into this ledger (incremental).
    Sync {
        /// Skip the mDNS lookup of paired devices' current addresses.
        #[arg(long)]
        no_discover: bool,
        /// Per-device time budget in seconds.
        #[arg(long, default_value_t = 45)]
        timeout: u64,
    },
    /// Viewers paired with this device, devices this one has paired with, and
    /// overlap warnings.
    List,
    /// Server side: revoke a viewer (by id or name); its token fails at once.
    Revoke { viewer: String },
    /// Viewer side: drop a paired device's pin and token.
    Forget { device: String },
}

fn print(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

/// One JSON failure document on stdout and a non-zero exit.
fn fail(operation: &str, code: &str) -> Result<()> {
    print(&peer::failure(operation, code))?;
    std::process::exit(1);
}

pub fn run(db: &Path, command: &Command) -> Result<()> {
    match command {
        Command::Serve {
            bind,
            name,
            plaintext,
            no_advertise,
        } => serve(db, bind, name.as_deref(), *plaintext, !*no_advertise),
        Command::Discover { timeout, .. } => {
            let (found, invalid) = discovery::discover(
                &Store::open(db)?,
                Duration::from_secs((*timeout).clamp(1, 60)),
            )?;
            print(
                &json!({"protocol": peer::PROTOCOL, "operation": "discover", "status": "ok",
                "service": discovery::SERVICE_TYPE, "devices": found, "invalid_answers": invalid}),
            )
        }
        Command::Pair { addr, code, name } => {
            let name = name.clone().unwrap_or_else(default_name);
            match pairing::pair(&Store::open(db)?, addr, code.trim(), &name)? {
                Ok(peer) => print(&json!({"protocol": peer::PROTOCOL,
                    "operation": "pair", "status": "ok", "peer": peer})),
                Err(code) => fail("pair", code),
            }
        }
        Command::Pull {
            target,
            endpoint,
            from,
            known,
            plaintext,
            no_discover,
            protocol_version,
            ..
        } => {
            let query = if endpoint == "summary" {
                let mut raw = Vec::new();
                if let Some(from) = from {
                    raw.push(format!("from={from}"));
                }
                if let Some(known) = known {
                    raw.push(format!("known={known}"));
                }
                let query = SummaryQuery::parse(&raw.join("&"))
                    .context("--from must be YYYY-MM-DD and --known DAY:FP[,DAY:FP…]")?;
                query.to_query()
            } else if from.is_some() || known.is_some() {
                anyhow::bail!("--from/--known only apply to --endpoint summary");
            } else {
                String::new()
            };
            match client::pull(
                &Store::open(db)?,
                target,
                *plaintext,
                !*no_discover,
                *protocol_version,
                endpoint,
                &query,
            )? {
                Ok((status, body)) => {
                    print(&body)?;
                    if status != 200 {
                        std::process::exit(1);
                    }
                    Ok(())
                }
                Err(code) => fail(endpoint, code),
            }
        }
        Command::Sync {
            no_discover,
            timeout,
        } => {
            let store = Store::open(db)?;
            let report = peer::sync::sync_all(
                &store,
                &peer::sync::SyncOptions {
                    rediscover: !*no_discover,
                    device_timeout: Duration::from_secs((*timeout).clamp(1, 600)),
                },
            )?;
            print(
                &json!({"protocol": peer::PROTOCOL, "operation": "sync", "status": "ok",
                "report": report, "warnings": peer::merged::overlaps(&store)?}),
            )
        }
        Command::List => {
            let store = Store::open(db)?;
            let trust = TrustStore::open(&peer_dir(db))?;
            let (local_id, local_name) = peer::local_device(&store)?;
            print(
                &json!({"protocol": peer::PROTOCOL, "operation": "list", "status": "ok",
                "device": {"device_id": local_id, "name": local_name},
                "viewers": trust.viewers()?, "peers": peer::devices::Registry::open(&store)?.list()?,
                "warnings": peer::merged::overlaps(&store)?}),
            )
        }
        Command::Revoke { viewer } => {
            let trust = TrustStore::open(&peer_dir(db))?;
            match trust.revoke(viewer, globaltokentracker_core::store::now_ms()) {
                Ok(Some(v)) => print(&json!({"protocol": peer::PROTOCOL,
                    "operation": "revoke", "status": "ok", "viewer": v})),
                Ok(None) => fail("revoke", "unknown_viewer"),
                Err(e) if e.is::<peer::trust::Ambiguous>() => fail("revoke", "ambiguous_viewer"),
                Err(e) => Err(e),
            }
        }
        Command::Forget { device } => {
            let store = Store::open(db)?;
            match peer::devices::Registry::open(&store)?.forget(device) {
                Ok(Some(p)) => print(&json!({"protocol": peer::PROTOCOL,
                    "operation": "forget", "status": "ok", "peer": p})),
                Ok(None) => fail("forget", "unknown_peer"),
                Err(e) if e.is::<peer::trust::Ambiguous>() => fail("forget", "ambiguous_peer"),
                Err(e) => Err(e),
            }
        }
    }
}

fn default_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|n| !n.trim().is_empty()))
        .unwrap_or_else(|| "viewer".into())
}

fn serve(
    db: &Path,
    bind: &str,
    name: Option<&str>,
    plaintext: bool,
    advertise: bool,
) -> Result<()> {
    // Refuse a plaintext non-loopback bind before touching anything.
    let listener = server::bind(bind, !plaintext)?;
    // The only ledger write: create the device id (and save --name) on the
    // normal read-write path. Requests then read through read-only
    // connections; pairing state lives in <data dir>/peer.
    let identity = {
        let store = Store::open(db)?;
        peer::ensure_identity(&store, name)?
    };
    let device = if plaintext {
        None
    } else {
        Some(DeviceKey::load_or_create(&peer_dir(db))?)
    };
    let state = Arc::new(ServerState::new(
        db.to_path_buf(),
        identity,
        device.as_ref(),
    )?);
    let local = listener.local_addr()?;
    eprintln!(
        "peer: serving {} v{} on {}://{local}/peer/v{}/ as {:?} ({})",
        peer::PROTOCOL,
        peer::PROTOCOL_VERSION,
        if plaintext { "http" } else { "https" },
        peer::PROTOCOL_VERSION,
        state.identity.device_name,
        state.identity.device_id
    );
    if let Some(device) = &device {
        eprintln!("peer: certificate fingerprint {}", device.fingerprint);
        show_code(&state, local)?;
        eprintln!("peer: press Enter for a new pairing code");
        let codes = Arc::clone(&state);
        std::thread::spawn(move || {
            let mut line = String::new();
            while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
                line.clear();
                let _ = show_code(&codes, local);
            }
        });
    } else {
        eprintln!("peer: plaintext loopback — pairing unavailable");
    }
    // Advertise only LAN TLS servers; withdraw (mDNS goodbye) on Ctrl-C/SIGTERM.
    let advert = Arc::new(Mutex::new(None));
    if advertise && !plaintext && !local.ip().is_loopback() {
        let txt = discovery::Txt::for_identity(&state.identity).context("peer: no fingerprint")?;
        match discovery::Advertisement::start(&txt, local) {
            Ok(ad) => {
                eprintln!("peer: advertising {} on mDNS", discovery::SERVICE_TYPE);
                *advert.lock().map_err(|_| anyhow::anyhow!("poisoned"))? = Some(ad);
            }
            Err(e) => {
                eprintln!("peer: mDNS advertisement unavailable ({e:#}); add this device by IP");
            }
        }
    }
    let on_exit = Arc::clone(&advert);
    ctrlc::set_handler(move || {
        if let Some(ad) = on_exit.lock().ok().and_then(|mut a| a.take()) {
            ad.withdraw();
            eprintln!("peer: mDNS advertisement withdrawn");
        }
        std::process::exit(0);
    })
    .context("peer: install signal handler")?;
    server::serve(&listener, state)
}

fn show_code(state: &ServerState, local: std::net::SocketAddr) -> Result<()> {
    let code = state.issue_code()?;
    eprintln!(
        "peer: pairing code {} {} — valid {} s, single use. On the viewer: \
         globaltokentracker-cli peer pair {} {code}",
        &code[..3],
        &code[3..],
        pairing::CODE_TTL.as_secs(),
        if local.ip().is_unspecified() {
            format!("<this device's IP>:{}", local.port())
        } else {
            local.to_string()
        }
    );
    Ok(())
}
