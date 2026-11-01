//! Importing a previous run's cache into the seed file: provider-seeded rows whose
//! authentication tags verify, and backfill markers — neither of which the log can rebuild.

use std::path::Path;

use anyhow::Context;
use engine_core::types::Millis;
use rusqlite::{TransactionBehavior, params};
use tracing::warn;

use crate::history::marks::Resolution;
use crate::history::marks::store::{Row, Store, read_only, store, tag};

impl Store {
    /// Imports the cache at `prior`, which a previous run left at `path`'s final name: provider-
    /// seeded rows whose authentication tags verify, and backfill markers, both with INSERT OR
    /// IGNORE, so log-derived rows the replay will re-derive never overwrite what the log proves.
    /// Rows the host altered fail verification and are dropped with a warning, never imported. A
    /// prior file of another schema version is skipped: this build cannot authenticate its rows,
    /// and the log re-derives what it proves while a configured backfill re-seeds the rest. One
    /// transaction; the prior file is opened read-only and never modified.
    /// Error cases:
    /// - the prior file cannot be read
    pub fn import_prior(&mut self, prior: &Path) -> anyhow::Result<usize> {
        if !prior.exists() {
            return Ok(0);
        }
        let from = read_only(prior).with_context(|| format!("opening the prior marks cache at {}", prior.display()))?;
        let version: i64 = from
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .with_context(|| format!("reading the prior marks cache at {}", prior.display()))?;
        if version != super::SCHEMA_VERSION {
            warn!(
                alert = "marks_prior_cache_dropped",
                version, "the prior marks cache holds another schema; it is not imported and will be rebuilt"
            );
            return Ok(0);
        }
        let rows = {
            let mut stmt = from
                .prepare("SELECT market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag FROM marks")
                .context("reading the prior marks cache")?;
            let mapped = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Millis>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, Option<Millis>>(8)?,
                        row.get::<_, [u8; 16]>(9)?,
                    ))
                })
                .context("reading the prior marks cache")?;
            mapped.collect::<Result<Vec<_>, _>>().context("reading the prior marks cache")?
        };
        let markers: Vec<(i64, String, String, [Millis; 8])> = {
            let mut stmt = from
                .prepare(
                    "SELECT market, venue, symbol, at, depth_1m, depth_1h, depth_1d, retention_1s, retention_1m, retention_1h, retention_1d FROM backfilled",
                )
                .context("reading the prior backfill markers")?;
            let mapped = stmt
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        [
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                            row.get(9)?,
                            row.get(10)?,
                        ],
                    ))
                })
                .context("reading the prior backfill markers")?;
            mapped.collect::<Result<Vec<_>, _>>().context("reading the prior backfill markers")?
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("starting the prior marks import")?;
        store::check_schema(&tx, &self.schema, &self.path)?;
        let mut imported = 0;
        let mut dropped = 0;
        {
            let mut insert = tx
                .prepare(
                    "INSERT OR IGNORE INTO marks (market, resolution, bucket, open, high, low, close, provenance, prev_bucket, tag)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                )
                .context("preparing the prior marks import")?;
            for (market, resolution, bucket, open, high, low, close, provenance, prev, stored) in rows {
                let resolution = match u8::try_from(resolution).ok().and_then(Resolution::from_tag) {
                    Some(resolution) => resolution,
                    None => {
                        dropped += 1;
                        continue;
                    }
                };
                let market = match u32::try_from(market) {
                    Ok(market) => market,
                    Err(_) => {
                        dropped += 1;
                        continue;
                    }
                };
                let expected = tag(
                    &self.mac,
                    market,
                    resolution,
                    bucket,
                    &Row { open: &open, high: &high, low: &low, close: &close, provenance: &provenance, prev },
                );
                if stored != expected {
                    dropped += 1;
                    continue;
                }
                imported += insert
                    .execute(params![
                        i64::from(market),
                        resolution as i64,
                        bucket,
                        open,
                        high,
                        low,
                        close,
                        provenance,
                        prev,
                        stored
                    ])
                    .with_context(|| format!("importing the prior {market} {resolution:?} bucket at {bucket}"))?;
            }
        }
        {
            let mut insert = tx
                .prepare(
                    "INSERT OR IGNORE INTO backfilled
                         (market, venue, symbol, at, depth_1m, depth_1h, depth_1d, retention_1s, retention_1m, retention_1h, retention_1d)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )
                .context("preparing the prior marker import")?;
            for (market, venue, symbol, [at, dm, dh, dd, rs, rm, rh, rd]) in markers {
                insert
                    .execute(params![market, venue, symbol, at, dm, dh, dd, rs, rm, rh, rd])
                    .context("importing a prior backfill marker")?;
            }
        }
        tx.commit().context("committing the prior marks import")?;
        if dropped > 0 {
            warn!(
                alert = "marks_prior_rows_dropped",
                dropped, "the prior marks cache held rows that fail authentication; they were not imported"
            );
        }
        self.refresh_anchors()?;
        Ok(imported)
    }

    /// Checkpoints the write-ahead log into the database file, so the cache is one file that
    /// activation can move into place.
    /// Error cases:
    /// - the checkpoint fails
    pub fn checkpoint(&self) -> anyhow::Result<()> {
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .context("checkpointing the marks cache")?;
        anyhow::ensure!(busy == 0, "the marks cache checkpoint did not complete");
        Ok(())
    }
}
