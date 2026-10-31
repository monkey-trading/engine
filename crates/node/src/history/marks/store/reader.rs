//! Reading pages of the marks cache: one bounded window per request, every row's authentication
//! tag verified before it is served.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};
use rusqlite::params;
use tokio::sync::Semaphore;
use tokio::task::spawn_blocking;

use crate::history::marks::store::{PageQuery, Row, SchemaRow, WINDOW, tag};
use crate::history::marks::{MarkEntry, MarksPage, Provenance, Resolution};
use crate::history::store::{self as store, read_only};

/// Reads pages of the cache, each on a new read-only connection.
#[derive(Clone)]
pub struct Reader {
    path: PathBuf,
    permits: Arc<Semaphore>,
    /// Verifies each row's authentication tag; the same key the writer derived it with.
    mac: hmac::Hmac<sha2::Sha256>,
    /// What `SCHEMA` creates, which every read checks the file against.
    schema: Arc<[SchemaRow]>,
}
impl Reader {
    /// A reader of the cache at `path`, authenticating rows with `mac`, on its own read-only
    /// connections; the store keeps writing for the task that owns it.
    pub(super) fn new(path: PathBuf, mac: hmac::Hmac<sha2::Sha256>) -> Self {
        Reader {
            path,
            permits: Arc::new(Semaphore::new(super::MAX_READS)),
            mac,
            schema: super::expected_schema().into(),
        }
    }

    /// One page of `market`'s marks at `resolution`, oldest first, over the inclusive window
    /// [`from`, `to`], at most `limit` entries plus the lookahead that decides `next`; `max_limit`
    /// is the response cap the page documents. It is read on a blocking thread holding one of
    /// `MAX_READS` permits, which it keeps until the read ends even if the request is dropped.
    /// Error cases:
    /// - the cache cannot be opened or read, holds another schema, or holds a row that does not parse
    pub async fn page(
        &self,
        market: MarketId,
        resolution: Resolution,
        from: Millis,
        to: Millis,
        limit: usize,
        max_limit: usize,
    ) -> anyhow::Result<MarksPage> {
        let permit = self.permits.clone().acquire_owned().await.expect("the read semaphore is never closed");
        let (path, schema, mac) = (self.path.clone(), self.schema.clone(), self.mac.clone());
        let query = PageQuery { market, resolution, from, to, limit, max_limit };
        let read = spawn_blocking(move || {
            let _permit = permit;
            read_page(&path, &schema, &mac, &query)
        });
        read.await.context("marks reader task")?
    }
}

/// Reads one page from the cache at `path`, once it holds exactly `schema`.
/// Error cases:
/// - the cache cannot be opened or read, holds another schema, or holds a row that does not parse
pub(super) fn read_page(
    path: &Path,
    schema: &[SchemaRow],
    mac: &hmac::Hmac<sha2::Sha256>,
    query: &PageQuery,
) -> anyhow::Result<MarksPage> {
    let PageQuery { market, resolution, from, to, limit, max_limit } = *query;
    let mut conn = read_only(path)?;
    // The schema check and the page read see one snapshot of the file.
    let conn = conn.transaction().context("starting a marks read")?;
    store::check_schema(&conn, schema, path)?;
    // One more than the page, to learn whether a newer page follows.
    let fetched = i64::try_from(limit).context("page limit")?.saturating_add(1);
    let mut stmt = conn.prepare(WINDOW).context("preparing a marks page")?;
    let rows = stmt
        .query_map(params![i64::from(market), resolution as i64, from, to, fetched], |row| {
            Ok((
                row.get::<_, Millis>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, [u8; 16]>(6)?,
            ))
        })
        .context("reading a marks page")?;
    let mut entries: Vec<MarkEntry> = rows
        .map(|row| {
            let (ts, open, high, low, close, provenance, stored) = row.context("reading a marks row")?;
            // A row the host rewrote fails authentication before it can be served.
            let expected = tag(
                mac,
                market,
                resolution,
                ts,
                &Row { open: &open, high: &high, low: &low, close: &close, provenance: &provenance },
            );
            ensure!(stored == expected, "a stored mark at {ts} fails authentication");
            let parse = |text: String| text.parse::<Fixed>().with_context(|| format!("a stored mark {text:?}"));
            let provenance =
                Provenance::parse(&provenance).with_context(|| format!("a stored provenance {provenance:?}"))?;
            Ok(MarkEntry {
                ts,
                open: parse(open)?,
                high: parse(high)?,
                low: parse(low)?,
                close: parse(close)?,
                provenance,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    let next = if entries.len() > limit {
        entries.truncate(limit);
        entries.last().map(|entry| entry.ts + 1)
    } else {
        None
    };
    Ok(MarksPage { market, resolution: resolution.as_str(), entries, next, max_limit })
}
