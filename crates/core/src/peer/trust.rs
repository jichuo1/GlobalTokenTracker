//! `peer.db` — who may read this device, and which devices this one reads.
//!
//! Server side (`viewers`): one row per paired viewer holding only the
//! SHA-256 of its bearer token, its name, created/last-seen and revocation
//! time. Viewer side (`peers`): the pinned certificate fingerprint, address
//! and bearer token of each paired device. Kept apart from the ledger so the
//! protocol's ledger reads stay read-only; the file lives in the owner-only
//! peer directory.

use super::keys::{ct_eq, ensure_private_dir, hex};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

const FILE: &str = "peer.db";
/// A code that failed on this viewer is refused for this long: a failure may
/// mean an interceptor learned part of it.
const FAILED_CODE_MS: i64 = 10 * 60_000;
/// `last_seen_at` is refreshed at most this often per viewer.
const TOUCH_MS: i64 = 60_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewer {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub fingerprint: String,
    pub device_id: String,
    pub device_name: String,
    pub address: String,
    #[serde(skip_serializing)]
    pub token: String,
    pub paired_at: i64,
    pub last_pulled_at: Option<i64>,
}

/// A selector matched more than one row; nothing was changed.
#[derive(Debug, thiserror::Error)]
#[error("ambiguous selector")]
pub struct Ambiguous;

pub struct TrustStore {
    conn: Connection,
}

pub(crate) fn token_hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

impl TrustStore {
    /// Open (creating owner-only if needed) `<dir>/peer.db`.
    pub fn open(dir: &Path) -> Result<Self> {
        ensure_private_dir(dir)?;
        let path = dir.join(FILE);
        if !path.exists() {
            super::keys::write_private(&path, &[])?;
        }
        let conn = Connection::open(&path).context("peer: open peer.db")?;
        conn.busy_timeout(Duration::from_secs(2))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS viewers(
                id TEXT PRIMARY KEY, name TEXT NOT NULL,
                token_sha256 TEXT NOT NULL UNIQUE, created_at INTEGER NOT NULL,
                last_seen_at INTEGER, revoked_at INTEGER);
             CREATE TABLE IF NOT EXISTS peers(
                fingerprint TEXT PRIMARY KEY, device_id TEXT NOT NULL,
                device_name TEXT NOT NULL, address TEXT NOT NULL, token TEXT NOT NULL,
                paired_at INTEGER NOT NULL, last_pulled_at INTEGER);
             CREATE TABLE IF NOT EXISTS pair_failures(
                code_sha256 TEXT PRIMARY KEY, at INTEGER NOT NULL);",
        )?;
        Ok(Self { conn })
    }

    // ── server side ────────────────────────────────────────────────────

    /// Register a viewer; only the token's hash is stored.
    pub fn add_viewer(&self, name: &str, token: &str, now: i64) -> Result<Viewer> {
        let id = hex(&super::pairing::random::<4>()?);
        self.conn.execute(
            "INSERT INTO viewers(id,name,token_sha256,created_at) VALUES(?1,?2,?3,?4)",
            params![id, name, token_hash(token), now],
        )?;
        Ok(Viewer {
            id,
            name: name.into(),
            created_at: now,
            last_seen_at: None,
            revoked_at: None,
        })
    }

    /// The viewer id a bearer token belongs to, if it is valid and not
    /// revoked. Compares against every active hash without an early exit.
    pub fn authorize(&self, token: &str, now: i64) -> Result<Option<String>> {
        let presented = token_hash(token);
        let mut st = self.conn.prepare(
            "SELECT id, token_sha256, last_seen_at FROM viewers WHERE revoked_at IS NULL",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut found = None;
        for (id, hash, seen) in rows {
            if ct_eq(hash.as_bytes(), presented.as_bytes()) {
                found = Some((id, seen));
            }
        }
        let Some((id, seen)) = found else {
            return Ok(None);
        };
        if seen.is_none_or(|s| now - s >= TOUCH_MS) {
            self.conn.execute(
                "UPDATE viewers SET last_seen_at=?2 WHERE id=?1",
                params![id, now],
            )?;
        }
        Ok(Some(id))
    }

    pub fn viewers(&self) -> Result<Vec<Viewer>> {
        let mut st = self.conn.prepare(
            "SELECT id,name,created_at,last_seen_at,revoked_at FROM viewers ORDER BY created_at,id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Viewer {
                id: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
                last_seen_at: r.get(3)?,
                revoked_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Revoke the active viewer whose id or name is `selector`. Takes effect
    /// on the very next request (every request re-reads this table).
    pub fn revoke(&self, selector: &str, now: i64) -> Result<Option<Viewer>> {
        let matches: Vec<Viewer> = self
            .viewers()?
            .into_iter()
            .filter(|v| v.revoked_at.is_none())
            .filter(|v| v.id == selector || v.name == selector)
            .collect();
        let chosen = match matches.as_slice() {
            [] => return Ok(None),
            [one] => one.clone(),
            many => match many.iter().find(|v| v.id == selector) {
                Some(v) => v.clone(),
                None => return Err(Ambiguous.into()),
            },
        };
        self.conn.execute(
            "UPDATE viewers SET revoked_at=?2 WHERE id=?1",
            params![chosen.id, now],
        )?;
        Ok(Some(Viewer {
            revoked_at: Some(now),
            ..chosen
        }))
    }

    // ── viewer side ────────────────────────────────────────────────────

    /// Save (or replace, on re-pairing) a paired device.
    pub fn save_peer(&self, peer: &Peer) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO peers(fingerprint,device_id,device_name,address,token,paired_at,last_pulled_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                peer.fingerprint,
                peer.device_id,
                peer.device_name,
                peer.address,
                peer.token,
                peer.paired_at,
                peer.last_pulled_at
            ],
        )?;
        Ok(())
    }

    pub fn peers(&self) -> Result<Vec<Peer>> {
        let mut st = self.conn.prepare(
            "SELECT fingerprint,device_id,device_name,address,token,paired_at,last_pulled_at
             FROM peers ORDER BY device_name,fingerprint",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Peer {
                fingerprint: r.get(0)?,
                device_id: r.get(1)?,
                device_name: r.get(2)?,
                address: r.get(3)?,
                token: r.get(4)?,
                paired_at: r.get(5)?,
                last_pulled_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The paired device `selector` names: full fingerprint (or a prefix of
    /// at least 8 characters), device id, device name or saved address.
    pub fn find_peer(&self, selector: &str) -> Result<Option<Peer>> {
        let selector_lc = selector.to_ascii_lowercase();
        let matches: Vec<Peer> = self
            .peers()?
            .into_iter()
            .filter(|p| {
                p.fingerprint == selector_lc
                    || (selector.len() >= 8 && p.fingerprint.starts_with(&selector_lc))
                    || p.device_id == selector
                    || p.device_name == selector
                    || p.address == selector
            })
            .collect();
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.into_iter().next()),
            _ => Err(Ambiguous.into()),
        }
    }

    /// Drop a paired device (its pin and token).
    pub fn forget(&self, selector: &str) -> Result<Option<Peer>> {
        let Some(peer) = self.find_peer(selector)? else {
            return Ok(None);
        };
        self.conn.execute(
            "DELETE FROM peers WHERE fingerprint=?1",
            [&peer.fingerprint],
        )?;
        Ok(Some(peer))
    }

    pub(crate) fn touch_peer(&self, fingerprint: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE peers SET last_pulled_at=?2 WHERE fingerprint=?1",
            params![fingerprint, now],
        )?;
        Ok(())
    }

    pub(crate) fn note_failed_code(&self, code: &str, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO pair_failures(code_sha256,at) VALUES(?1,?2)",
            params![token_hash(code), now],
        )?;
        self.conn.execute(
            "DELETE FROM pair_failures WHERE at < ?1",
            [now - FAILED_CODE_MS],
        )?;
        Ok(())
    }

    pub(crate) fn code_failed_recently(&self, code: &str, now: i64) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM pair_failures WHERE code_sha256=?1 AND at >= ?2",
                params![token_hash(code), now - FAILED_CODE_MS],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}
