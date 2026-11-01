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

use crate::history::marks::store::chain::verify_row;
use crate::history::marks::store::{
    ChainAnchor, ChainAnchors, ChainHead, PageQuery, Row, SchemaRow, SnapRow, WINDOW, tag,
};
use crate::history::marks::{MarkEntry, MarksPage, Provenance, Resolution};
use crate::history::store::{self as store, read_only};

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

/// The stale-anchor race: a commit published between one attempt's snapshot and its anchor
/// read, so the two belong to different commits. `read_page` re-synchronizes with a fresh
/// snapshot and anchor; a mismatch that survives the retries is a rollback.
#[derive(Debug)]
pub(crate) struct StaleAnchor;

impl std::fmt::Display for StaleAnchor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the committed chain anchor and the snapshot come from different commits")
    }
}

impl std::error::Error for StaleAnchor {}

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
    read_page_with(path, schema, mac, anchors, query, &mut |_| {})
}

/// `read_page`, calling `after_snapshot` with the attempt's number (from 0) once the attempt holds
/// its snapshot and before it reads the committed anchor: the window a racing commit lands in,
/// which a test fills with a real commit.
/// Error cases:
/// - as for `read_page`
pub(super) fn read_page_with(
    path: &Path,
    schema: &[SchemaRow],
    mac: &hmac::Hmac<sha2::Sha256>,
    anchors: &Arc<Mutex<ChainAnchors>>,
    query: &PageQuery,
    after_snapshot: &mut dyn FnMut(usize),
) -> anyhow::Result<MarksPage> {
    // A commit publishes its anchors atomically, so a mismatch still means one commit landed
    // inside an attempt's window between its snapshot and its anchor read: re-synchronize, twice
    // if needed, with a fresh snapshot and a fresh anchor each time, before a mismatch is a
    // rollback.
    for attempt in 0..2 {
        match read_page_once(path, schema, mac, anchors, query, true, &mut || after_snapshot(attempt)) {
            Err(e) if e.downcast_ref::<StaleAnchor>().is_some() => {}
            result => return result,
        }
    }
    read_page_once(path, schema, mac, anchors, query, false, &mut || after_snapshot(2))
}

/// One attempt at `read_page`, on one snapshot of the file. With `allow_stale`, an anchor and
/// snapshot from different commits reports the stale-anchor race so the caller can re-
/// synchronize; otherwise a mismatch is a rollback. `after_snapshot` runs once the snapshot is
/// held.
/// Error cases:
/// - the cache cannot be opened or read, holds another schema, holds a row that does not parse,
///   or its chain fails verification; with `allow_stale`, a raced commit reports the stale-anchor
///   race
pub(crate) fn read_page_once(
    path: &Path,
    schema: &[SchemaRow],
    mac: &hmac::Hmac<sha2::Sha256>,
    anchors: &Arc<Mutex<ChainAnchors>>,
    query: &PageQuery,
    allow_stale: bool,
    after_snapshot: &mut dyn FnMut(),
) -> anyhow::Result<MarksPage> {
    let PageQuery { market, resolution, from, to, limit, max_limit } = *query;
    let mut conn = read_only(path)?;
    // The schema check and the page read see one snapshot of the file.
    let conn = conn.transaction().context("starting a marks read")?;
    store::check_schema(&conn, schema, path)?;
    after_snapshot();
    let anchor =
        anchors.lock().expect("the marks chain anchors are never poisoned").get(&(market, resolution)).copied();
    let ends: Option<ChainAnchor> = conn
        .query_row(
            "SELECT
                 (SELECT bucket FROM marks WHERE market = :market AND resolution = :resolution ORDER BY bucket ASC LIMIT 1),
                 (SELECT tag FROM marks WHERE market = :market AND resolution = :resolution ORDER BY bucket ASC LIMIT 1),
                 (SELECT bucket FROM marks WHERE market = :market AND resolution = :resolution ORDER BY bucket DESC LIMIT 1),
                 (SELECT tag FROM marks WHERE market = :market AND resolution = :resolution ORDER BY bucket DESC LIMIT 1)",
            params![i64::from(market), resolution as i64],
            // A malformed tag (the host can write any BLOB the loose schema accepts) fails the
            // read like any other corruption; it never panics the node.
            |row| {
                let floor: Option<(Millis, [u8; 16])> = row.get::<_, Option<Millis>>(0)?.zip(row.get(1)?);
                let head: Option<(Millis, [u8; 16])> = row.get::<_, Option<Millis>>(2)?.zip(row.get(3)?);
                Ok(floor.zip(head).map(|(floor, head)| ChainAnchor {
                    floor: ChainHead { bucket: floor.0, tag: floor.1 },
                    head: ChainHead { bucket: head.0, tag: head.1 },
                }))
            },
        )
        .context("reading the marks chain ends")?;
    match (anchor, ends) {
        (Some(anchor), Some(ends)) => {
            // A commit publishes its anchors atomically, so a file state and an anchor from
            // different commits differ only by a commit landing inside this attempt's window —
            // the shape both a race and a rollback take. Re-synchronize before calling a
            // mismatch a rollback.
            if anchor != ends && allow_stale {
                return Err(StaleAnchor.into());
            }
            ensure!(
                anchor == ends,
                "the {market} {resolution:?} marks were rolled back: the file's chain floor or head does not match the committed anchor"
            );
        }
        // Pair creation and removal publish atomically with a commit too, so these shapes are a
        // race on a stale-tolerant attempt and tampering on the last one, as with a mismatch.
        (Some(_), None) if allow_stale => return Err(StaleAnchor.into()),
        (None, Some(_)) if allow_stale => return Err(StaleAnchor.into()),
        (Some(_), None) => {
            bail!("the {market} {resolution:?} marks lost every bucket below the committed chain anchor")
        }
        (None, Some(_)) => {
            bail!("the {market} {resolution:?} cache holds rows no committed chain anchor names")
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
    // refused by the same read. A truncated page already verified its lookahead row in the loop
    // above; the rows it did not fetch are the next page's to vouch for, so the row above `to`
    // may name one of them.
    if next.is_none()
        && let Some(above) = &above
    {
        let expected = entries.last().map(|entry| entry.ts).or(below.as_ref().map(|below| below.bucket));
        ensure!(
            above.prev == expected,
            "a stored mark at {} breaks the chain: its link does not name the bucket below it",
            above.bucket
        );
    }
    Ok(MarksPage { market, resolution: resolution.as_str(), entries, next, max_limit })
}
