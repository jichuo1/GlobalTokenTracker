use anyhow::Result;
use clap::Subcommand;
use globaltokentracker_core::coordination::{self, SnapshotOptions};
use std::path::Path;

#[derive(Subcommand)]
pub enum Command {
    /// Protocol negotiation, registered metering sources and refresh support.
    Capabilities,
    /// One coherent, read-only ledger projection; no scan or network requests.
    Snapshot {
        #[arg(long)]
        source: Option<String>,
        #[arg(long, default_value_t = 14)]
        lookback_days: u32,
        #[arg(long, default_value_t = 300)]
        quota_ttl_secs: u32,
        #[arg(long, default_value_t = 512)]
        model_limit: usize,
        #[arg(long, default_value_t = 256)]
        quota_limit: usize,
    },
    /// Explicit mutation. --quota also permits the selected vendor's network/auth refresh.
    Refresh {
        #[arg(long, required_unless_present = "all", conflicts_with = "all")]
        source: Option<String>,
        #[arg(long, required_unless_present = "source")]
        all: bool,
        #[arg(long)]
        quota: bool,
        #[arg(long, default_value_t = 30)]
        min_interval_secs: u32,
    },
}

pub fn run(db: Option<&Path>, command: &Command, version: u32) -> Result<()> {
    let path = db
        .map(Path::to_path_buf)
        .unwrap_or_else(coordination::readonly_db_path);
    let operation = match command {
        Command::Capabilities => "capabilities",
        Command::Snapshot { .. } => "snapshot",
        Command::Refresh { .. } => "refresh",
    };
    let value = if version != coordination::PROTOCOL_VERSION {
        coordination::failure(operation, "unsupported_protocol_version")
    } else {
        match command {
            Command::Capabilities => coordination::capabilities(),
            Command::Snapshot {
                source,
                lookback_days,
                quota_ttl_secs,
                model_limit,
                quota_limit,
            } => coordination::snapshot(
                &path,
                &SnapshotOptions {
                    source: source.clone(),
                    lookback_days: *lookback_days,
                    quota_ttl_secs: *quota_ttl_secs,
                    model_limit: *model_limit,
                    quota_limit: *quota_limit,
                },
            ),
            Command::Refresh {
                source,
                all,
                quota,
                min_interval_secs,
            } => coordination::refresh(&path, source.as_deref(), *all, *quota, *min_interval_secs),
        }
    };
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
}
