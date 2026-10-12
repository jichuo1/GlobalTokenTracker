//! Viewer side: the devices this ledger has paired with, and their pulled
//! data. The ledger's `peers` table is the single source of truth for a
//! paired device (name, pinned fingerprint, address, sync status); only the
//! bearer token lives elsewhere, in the owner-only `peer.db`. Peer data sits
//! in `peer_groups` / `peer_day_fps` / `peer_quotas` — never in
//! `usage_events` or `daily_rollups`, and never served onward.

use super::keys::peer_dir;
use super::trust::{Ambiguous, TrustStore};
use crate::store::Store;
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerDevice {
    pub device_id: String,
    pub name: String,
    /// Pinned certificate SHA-256 — the device's trust identity.
    pub fingerprint: String,
    pub last_address: String,
    pub paired_at: i64,
    pub last_seen_at: Option<i64>,
    pub last_pull_at: Option<i64>,
    pub last_error_code: Option<String>,
    pub gtt_version: Option<String>,
    pub protocol_version: Option<u32>,
    pub enabled: bool,
}

const COLUMNS: &str = "device_id,name,fingerprint,last_address,paired_at,last_seen_at,\
    last_pull_at,last_error_code,gtt_version,protocol_version,enabled";

fn row(r: &rusqlite::Row) -> rusqlite::Result<PeerDevice> {
    Ok(PeerDevice {
        device_id: r.get(0)?,
        name: r.get(1)?,
        fingerprint: r.get(2)?,
        last_address: r.get(3)?,
        paired_at: r.get(4)?,
        last_seen_at: r.get(5)?,
        last_pull_at: r.get(6)?,
        last_error_code: r.get(7)?,
        gtt_version: r.get(8)?,
        protocol_version: r.get(9)?,
        enabled: r.get::<_, i64>(10)? != 0,
    })
}

/// The ledger's peer registry plus the token store next to it.
pub struct Registry<'a> {
    pub(crate) store: &'a Store,
    pub(crate) trust: TrustStore,
}

impl<'a> Registry<'a> {
    /// Open for a file-backed ledger; moves step-2 viewer rows (kept in
    /// `peer.db`) into the ledger once.
    pub fn open(store: &'a Store) -> Result<Self> {
        let path = store
            .path()
            .context("peer: an in-memory ledger has no peer directory")?;
        let trust = TrustStore::open(&peer_dir(path))?;
        let registry = Self { store, trust };
        registry.migrate_legacy()?;
        Ok(registry)
    }

    fn migrate_legacy(&self) -> Result<()> {
        let legacy = self.trust.legacy_peers()?;
        if legacy.is_empty() {
            return self.trust.drop_legacy_peers();
        }
        let tx = self.store.conn().unchecked_transaction()?;
        for p in &legacy {
            tx.execute(
                "INSERT OR IGNORE INTO peers(device_id,name,fingerprint,last_address,paired_at,last_pull_at)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    p.device_id,
                    p.device_name,
                    p.fingerprint,
                    p.address,
                    p.paired_at,
                    p.last_pulled_at
                ],
            )?;
        }
        tx.commit()?;
        for p in &legacy {
            if self.trust.token(&p.device_id)?.is_none() {
                self.trust.set_token(&p.device_id, &p.token)?;
            }
        }
        self.trust.drop_legacy_peers()
    }

    pub fn list(&self) -> Result<Vec<PeerDevice>> {
        list(self.store)
    }

    /// The paired device `selector` names: full fingerprint (or a prefix of
    /// at least 8 characters), device id, name or last address.
    pub fn find(&self, selector: &str) -> Result<Option<PeerDevice>> {
        let lc = selector.to_ascii_lowercase();
        let matches: Vec<PeerDevice> = self
            .list()?
            .into_iter()
            .filter(|p| {
                p.fingerprint == lc
                    || (selector.len() >= 8 && p.fingerprint.starts_with(&lc))
                    || p.device_id == selector
                    || p.name == selector
                    || p.last_address == selector
            })
            .collect();
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.into_iter().next()),
            _ => Err(Ambiguous.into()),
        }
    }

    pub fn token(&self, device_id: &str) -> Result<Option<String>> {
        self.trust.token(device_id)
    }

    /// Save a freshly paired device (re-pairing replaces pin, name, address
    /// and token; pulled data is kept only if the certificate is unchanged).
    pub fn save_paired(&self, device: &PeerDevice, token: &str) -> Result<()> {
        let tx = self.store.conn().unchecked_transaction()?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT fingerprint FROM peers WHERE device_id=?1",
                [&device.device_id],
                |r| r.get(0),
            )
            .optional()?;
        if previous.is_some_and(|fp| fp != device.fingerprint) {
            delete_data(&tx, &device.device_id)?;
        }
        // Another device id with this certificate is the same device renamed.
        let stale: Vec<String> = {
            let mut st =
                tx.prepare("SELECT device_id FROM peers WHERE fingerprint=?1 AND device_id<>?2")?;
            st.query_map(params![device.fingerprint, device.device_id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for id in &stale {
            delete_data(&tx, id)?;
            tx.execute("DELETE FROM peers WHERE device_id=?1", [id])?;
        }
        tx.execute(
            "INSERT INTO peers(device_id,name,fingerprint,last_address,paired_at,enabled)
             VALUES(?1,?2,?3,?4,?5,1)
             ON CONFLICT(device_id) DO UPDATE SET name=excluded.name,
               fingerprint=excluded.fingerprint, last_address=excluded.last_address,
               paired_at=excluded.paired_at, last_error_code=NULL, enabled=1",
            params![
                device.device_id,
                device.name,
                device.fingerprint,
                device.last_address,
                device.paired_at
            ],
        )?;
        tx.commit()?;
        for id in &stale {
            self.trust.delete_token(id)?;
        }
        self.trust.set_token(&device.device_id, token)
    }

    /// Forget a device: its registry row and every pulled row go in one
    /// transaction, then its token.
    pub fn forget(&self, selector: &str) -> Result<Option<PeerDevice>> {
        let Some(device) = self.find(selector)? else {
            return Ok(None);
        };
        let tx = self.store.conn().unchecked_transaction()?;
        delete_data(&tx, &device.device_id)?;
        tx.execute("DELETE FROM peers WHERE device_id=?1", [&device.device_id])?;
        tx.commit()?;
        self.trust.delete_token(&device.device_id)?;
        Ok(Some(device))
    }

    pub fn set_enabled(&self, device_id: &str, enabled: bool) -> Result<bool> {
        Ok(self.store.conn().execute(
            "UPDATE peers SET enabled=?2 WHERE device_id=?1",
            params![device_id, i64::from(enabled)],
        )? > 0)
    }

    pub(crate) fn set_address(&self, device_id: &str, address: &str) -> Result<()> {
        self.store.conn().execute(
            "UPDATE peers SET last_address=?2 WHERE device_id=?1",
            params![device_id, address],
        )?;
        Ok(())
    }
}

/// Paired devices (no token access needed).
pub fn list(store: &Store) -> Result<Vec<PeerDevice>> {
    let mut st = store.conn().prepare(&format!(
        "SELECT {COLUMNS} FROM peers ORDER BY name, device_id"
    ))?;
    let rows = st.query_map([], row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn delete_data(conn: &rusqlite::Connection, device_id: &str) -> rusqlite::Result<()> {
    for table in ["peer_groups", "peer_day_fps", "peer_quotas"] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE device_id=?1"),
            [device_id],
        )?;
    }
    Ok(())
}
