use anyhow::{Context, Result};
use clap::Subcommand;
use globaltokentracker_core::Store;
use globaltokentracker_core::peer::{
    self, SummaryQuery,
    server::{self, ServerState},
};
use std::net::TcpListener;
use std::path::Path;

#[derive(Subcommand)]
pub enum Command {
    /// Serve this ledger's aggregates read-only (gtt.peer protocol, GET only).
    Serve {
        /// Listen address. Loopback by default; anything else exposes
        /// unauthenticated aggregates to the network (no TLS/pairing yet).
        #[arg(long, default_value = server::DEFAULT_BIND)]
        bind: String,
        /// Device display name to save and announce in `hello`.
        #[arg(long)]
        name: Option<String>,
    },
    /// Fetch one endpoint from a peer and print its JSON document.
    Pull {
        /// Peer address: host[:port] (default port 4319).
        addr: String,
        #[arg(long, default_value = "hello", value_parser = peer::ENDPOINTS)]
        endpoint: String,
        /// Summary only: UTC day lower bound, YYYY-MM-DD.
        #[arg(long)]
        from: Option<String>,
        /// Summary only: fingerprints already held, DAY:FP[,DAY:FP…].
        #[arg(long)]
        known: Option<String>,
        #[arg(long, default_value = "json", value_parser = ["json"])]
        format: String,
        #[arg(long, default_value_t = peer::PROTOCOL_VERSION)]
        protocol_version: u32,
    },
}

pub fn run(db: &Path, command: &Command) -> Result<()> {
    match command {
        Command::Serve { bind, name } => serve(db, bind, name.as_deref()),
        Command::Pull {
            addr,
            endpoint,
            from,
            known,
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
            let (_, body) = server::pull(addr, *protocol_version, endpoint, &query)?;
            println!("{}", serde_json::to_string(&body)?);
            Ok(())
        }
    }
}

fn serve(db: &Path, bind: &str, name: Option<&str>) -> Result<()> {
    // The only write: create the device id (and save --name) on the normal
    // read-write path. Every request afterwards reads through a read-only
    // connection.
    let identity = {
        let store = Store::open(db)?;
        peer::ensure_identity(&store, name)?
    };
    let listener = TcpListener::bind(bind).with_context(|| format!("peer: bind {bind}"))?;
    let local = listener.local_addr()?;
    if !local.ip().is_loopback() {
        eprintln!(
            "peer: WARNING listening on {local} — any device that can reach this address can read \
             this ledger's usage aggregates (no TLS or pairing in protocol v1)"
        );
    }
    eprintln!(
        "peer: serving {} v{} on http://{local}/peer/v{}/ as {:?} ({})",
        peer::PROTOCOL,
        peer::PROTOCOL_VERSION,
        peer::PROTOCOL_VERSION,
        identity.device_name,
        identity.device_id
    );
    server::serve(
        &listener,
        ServerState {
            db: db.to_path_buf(),
            identity,
        },
    )
}
