//! Reading pages of the marks cache: one bounded window per request, every row's authentication
//! tag and chained-index link verified against the committed chain heads before it is served, so
//! a host that deletes, reorders, or rolls the cache back is refused mid-run, at serve time.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail, ensure};
use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};
use rusqlite::{OptionalExtension, params};
use tokio::sync::Semaphore;
use tokio::task::spawn_blocking;

use crate::history::marks::store::{ChainAnchors, PageQuery, Row, SchemaRow, SnapRow, WINDOW, tag, verify_row};
use crate::history::marks::{MarkEntry, MarksPage, Provenance, Resolution};
use crate::history::store::{self as store, read_only};

/// The file's newest row of one pair: the chain head a page read checks before serving anything.
const HEAD: &str = "SELECT bucket, tag FROM marks
    WHERE market = :market AND resolution = :resolution ORDER BY bucket DESC LIMIT 1";
/// The row just below a window: the predecessor the window's first row must name.
const BELOW: &str = "SELECT bucket, open, high, low, close, provenance, prev_bucket, tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket < :from ORDER BY bucket DESC LIMIT 1";
/// The row just above a window: the successor whose link must name the window's last row.
const ABOVE: &str = "SELECT bucket, open, high, low, close, provenance, prev_bucket, tag FROM marks
    WHERE market = :market AND resolution = :resolution AND bucket > :to ORDER BY bucket ASC LIMIT 1";

/// Reads pages of the cache, each on a new read-only connection.
#[derive(Clone)]
pub struct Reader {
    path: PathBuf,
    permits: Arc<Semaphore>,
    /// Verifies each row's authentication tag; the same key the writer derived it with.
    mac: hmac::Hmac<sha2::Sha256>,
    /// What `SCHEMA` creates, which every read checks the file against.
    schema: Arc<[SchemaRow]>,
    /// The committed chain heads, shared with the writer: a page is served only against the
    /// head the store advanced, so a rolled-back file fails before any entry leaves.
    anchors: Arc<Mutex<ChainAnchors>>,
}
impl Reader {
    /// A reader of the cache at `path`, authenticating rows with `mac` and checking chains
    /// against `anchors`, on its own read-only connections; the store keeps writing for the task
    /// that owns it.
    pub(super) fn new(path: PathBuf, mac: hmac::Hmac<sha2::Sha256>, anchors: Arc<Mutex<ChainAnchors>>) -> Self {
        Reader {
            path,
            permits: Arc::new(Semaphore::new(super::MAX_READS)),
            mac,
            schema: super::expected_schema().into(),
            anchors,
        }
    }

    /// One page of `market`'s marks at `resolution`, oldest first, over the inclusive window
    /// [`from`, `to`], at most `limit` entries plus the lookahead that decides `next`; `max_limit`
    /// is the response cap the page documents. Every row's tag and chain link is verified, and
    /// the pair's chain head checked against the committed anchors, before anything is served. It
    /// is read on a blocking thread holding one of `MAX_READS` permits, which it keeps until the
    /// read ends even if the request is dropped.
    /// Error cases:
    /// - the cache cannot be opened or read, holds another schema, holds a row that does not
    ///   parse, or its chain fails verification
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
        let (path, schema, mac, anchors) =
            (self.path.clone(), self.schema.clone(), self.mac.clone(), self.anchors.clone());
        let query = PageQuery { market, resolution, from, to, limit, max_limit };
        let read = spawn_blocking(move || {
            let _permit = permit;
            read_page(&path, &schema, &mac, &anchors, &query)
        });
        read.await.context("marks reader task")?
    }
}

/// Reads one page from the cache at `path`, once it holds exactly `schema`.
/// Error cases:
/// - the cache cannot be opened or read, holds another schema, holds a row that does not parse,
///   or its chain fails verification
pub(super) fn read_page(
    path: &Path,
    schema: &[SchemaRow],
    mac: &hmac::Hmac<sha2::Sha256>,
    anchors: &Arc<Mutex<ChainAnchors>>,
    query: &PageQuery,
) -> anyhow::Result<MarksPage> {
    let PageQuery { market, resolution, from, to, limit, max_limit } = *query;
    let mut conn = read_only(path)?;
    // The schema check and the page read see one snapshot of the file.
    let conn = conn.transaction().context("starting a marks read")?;
    store::check_schema(&conn, schema, path)?;
    let anchor =
        anchors.lock().expect("the marks chain anchors are never poisoned").get(&(market, resolution)).copied();
    let head: Option<(Millis, [u8; 16])> = conn
        .query_row(HEAD, params![i64::from(market), resolution as i64], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()
        .context("reading the marks chain head")?;
    match (anchor, head) {
        (Some(anchor), Some((bucket, tag))) => ensure!(
            anchor.bucket == bucket && anchor.tag == tag,
            "the {market} {resolution:?} marks were rolled back: the file's chain head does not match the committed one"
        ),
        (Some(_), None) => bail!("the {market} {resolution:?} marks lost every bucket below the committed chain head"),
        (None, Some(_)) => {
            bail!("the {market} {resolution:?} cache holds rows no committed chain head names")
        }
        (None, None) => {}
    }
    let below: Option<SnapRow> = conn
        .query_row(BELOW, params![i64::from(market), resolution as i64, from], |row| {
            Ok(SnapRow {
                bucket: row.get(0)?,
                open: row.get(1)?,
                high: row.get(2)?,
                low: row.get(3)?,
                close: row.get(4)?,
                provenance: row.get(5)?,
                prev: row.get(6)?,
                tag: row.get(7)?,
            })
        })
        .optional()
        .context("reading below the marks window")?;
    if let Some(below) = &below {
        verify_row(mac, market, resolution, below).context("reading below the marks window")?;
    }
    let above: Option<SnapRow> = conn
        .query_row(ABOVE, params![i64::from(market), resolution as i64, to], |row| {
            Ok(SnapRow {
                bucket: row.get(0)?,
                open: row.get(1)?,
                high: row.get(2)?,
                low: row.get(3)?,
                close: row.get(4)?,
                provenance: row.get(5)?,
                prev: row.get(6)?,
                tag: row.get(7)?,
            })
        })
        .optional()
        .context("reading above the marks window")?;
    if let Some(above) = &above {
        verify_row(mac, market, resolution, above).context("reading above the marks window")?;
    }
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
                row.get::<_, Option<Millis>>(6)?,
                row.get::<_, [u8; 16]>(7)?,
            ))
        })
        .context("reading a marks page")?;
    let mut previous = below.as_ref().map(|below| below.bucket);
    let mut entries: Vec<MarkEntry> = rows
        .map(|row| {
            let (ts, open, high, low, close, provenance, prev, stored) = row.context("reading a marks row")?;
            // A row the host rewrote fails authentication before it can be served.
            let expected = tag(
                mac,
                market,
                resolution,
                ts,
                &Row { open: &open, high: &high, low: &low, close: &close, provenance: &provenance, prev },
            );
            ensure!(stored == expected, "a stored mark at {ts} fails authentication");
            // The chain link must name the bucket actually below the row, so a deletion or a
            // reorder breaks the page that spans it, and a floor row chains from nothing.
            ensure!(
                prev == previous,
                "a stored mark at {ts} breaks the chain: its link does not name the bucket below it"
            );
            previous = Some(ts);
            let parse = |text: String| text.parse::<Fixed>().with_context(|| format!("a stored mark {text:?}"));
            Ok(MarkEntry {
                ts,
                open: parse(open)?,
                high: parse(high)?,
                low: parse(low)?,
                close: parse(close)?,
                provenance: Provenance::parse(&provenance)
                    .with_context(|| format!("a stored provenance {provenance:?}"))?,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    let next = if entries.len() > limit {
        entries.truncate(limit);
        entries.last().map(|entry| entry.ts + 1)
    } else {
        None
    };
    // The successor above the window closes the span this read vouches for: its link must name
    // the window's last row, or the row below an empty window, so a deletion just above `to` is
    // refused by the same read.
    if let Some(above) = &above {
        let expected = entries.last().map(|entry| entry.ts).or(below.as_ref().map(|below| below.bucket));
        ensure!(
            above.prev == expected,
            "a stored mark at {} breaks the chain: its link does not name the bucket below it",
            above.bucket
        );
    }
    Ok(MarksPage { market, resolution: resolution.as_str(), entries, next, max_limit })
}
