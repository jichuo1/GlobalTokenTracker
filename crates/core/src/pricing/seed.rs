//! Offline price seed: a gzipped models.dev snapshot bundled into the binary
//! (spec §7.4 — first-run/offline must still price). Real `models.dev` sync
//! rows overwrite seed entries because `Store::load` orders seed last-wins...

use crate::store::Store;
use anyhow::Result;
use flate2::read::GzDecoder;
use rusqlite::params;
use std::io::Read;

/// `{model_id: [input, output, cache_read, cache_write]}` in $/1M.
const SEED_GZ: &[u8] = include_bytes!("../../assets/models_dev_seed.json.gz");

/// Import the bundled snapshot when `prices` is empty (first run / fresh DB).
pub fn ensure_seeded(store: &Store) -> Result<()> {
    let n: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM prices", [], |r| r.get(0))?;
    if n > 0 {
        return Ok(());
    }
    let mut raw = String::new();
    GzDecoder::new(SEED_GZ).read_to_string(&mut raw)?;
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw)?;
    let now = super::super::store::now_ms();
    // SAVEPOINT works both standalone and when called lazily inside an outer
    // transaction (e.g. CodeBuddy / Coordinator atomic ingest).
    store.conn().execute_batch("SAVEPOINT gtt_seed_prices")?;
    let res: Result<()> = (|| {
        let mut st = store.conn().prepare(
            "INSERT OR IGNORE INTO prices(provider, model_id, input, output, cache_read, cache_write, source, fetched_at)
             VALUES ('seed', ?1, ?2, ?3, ?4, ?5, 'seed', ?6)",
        )?;
        for (model_id, v) in &map {
            let a = v.as_array();
            let get = |i: usize| a.and_then(|a| a.get(i)).and_then(serde_json::Value::as_f64);
            st.execute(params![
                model_id,
                get(0).unwrap_or(0.0),
                get(1).unwrap_or(0.0),
                get(2).unwrap_or(0.0),
                get(3).unwrap_or(0.0),
                now
            ])?;
        }
        Ok(())
    })();
    match res {
        Ok(()) => store
            .conn()
            .execute_batch("RELEASE SAVEPOINT gtt_seed_prices")?,
        Err(e) => {
            let _ = store.conn().execute_batch(
                "ROLLBACK TO SAVEPOINT gtt_seed_prices; RELEASE SAVEPOINT gtt_seed_prices",
            );
            return Err(e);
        }
    }
    tracing::info!(models = map.len(), "price seed imported");
    Ok(())
}
