//! A listed segment's bytes from the archive: rebuilt from the bucket's ranges, or served by a node origin, and taken
//! only as the bytes its listing names. The window reads a segment back this way before it leaves the host, and puts
//! one back on the host that the host lost; a start that replays the whole log puts back every segment the window
//! removed (`restore_whole`), and a new or discarded history index the segments it takes (`history_start`). Every
//! restore writes the lacking segments newest first, so the host's segments stay one run however far it gets.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail, ensure};
use reqwest::{Client, StatusCode};
use tokio::task::spawn_blocking;
use tracing::{debug, error, info, warn};

use crate::archive::read::{Ranges, Unread};
use crate::archive::window::{Floors, FreshStart, MAX_RESTORES, Window, fresh_start};
use crate::archive::{Bucket, RANGE_SEQS, Store};
use crate::config::NodeConfig;
use crate::log::{MANIFEST, Manifest, SegmentInfo, list_segments};
use crate::standby::MAX_ARCHIVE_BYTES;
use crate::standby::archive::{Fetched, check_listing, fetch_from, validate_listing};

/// What a segment's file name is followed by while its restored bytes are written, then this process's run (`RUN`)
/// and a count of its restores, so concurrent restores of one segment each write a file of their own; no reader of the
/// log takes such a file as a segment (`log::segment_seq`).
const RESTORING: &str = ".restoring.";
/// A number this process drew at random, naming its staged files apart from an earlier process's, which a guest that
/// starts the same way each boot may have run under the same process id.
static RUN: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| rand::RngCore::next_u64(&mut rand::rngs::OsRng));
/// Restores this process has staged.
static STAGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Where a segment the host lacks is read: the bucket, then node origins in order.
#[derive(Clone, Debug)]
pub(crate) struct Sources {
    pub http: Client,
    pub bucket: Bucket,
    pub nodes: Vec<String>,
}

impl Sources {
    /// The bucket `cfg`'s `[archive]` names, and the archive mirrors its `[tee.sealing.handover]` and `[standby]`
    /// name, then the operator `[standby]` follows; none without `[archive]`.
    /// Error cases:
    /// - the bucket's URL or the client does not build
    pub(crate) fn for_config(cfg: &NodeConfig) -> anyhow::Result<Option<Self>> {
        let Some(bucket) = Bucket::for_config(cfg)? else { return Ok(None) };
        let handover = cfg.tee.sealing.as_ref().and_then(|sealing| sealing.handover.as_ref());
        let mirrors = handover.map(|handover| handover.mirrors.clone()).unwrap_or_default().into_iter();
        let standby = cfg.standby.iter().flat_map(|standby| standby.mirrors.iter().chain([&standby.operator]));
        let mut nodes: Vec<String> = Vec::new();
        for origin in mirrors.chain(standby.cloned()) {
            if !nodes.contains(&origin) {
                nodes.push(origin);
            }
        }
        Ok(Some(Self { http: Store::client()?, bucket, nodes }))
    }

    /// `listed`'s bytes read back from the bucket's ranges, as its listing names them.
    /// Error cases:
    /// - the bucket lacks a range of it, cannot be read, or gives back other bytes than the listing names
    pub(crate) async fn read_back(&self, listed: &SegmentInfo) -> anyhow::Result<Vec<u8>> {
        let mut ranges = Ranges::new(self.bucket.clone());
        // Bounded apart from the listing, which a whole replay's manifest, the host's, names.
        let limit = listed.bytes.min(crate::archive::read::MAX_REBUILT_BYTES);
        let bytes = match ranges.seqs(&self.http, (listed.first_seq, listed.last_seq), limit).await {
            Ok(bytes) => bytes,
            Err(Unread::Missing { object, .. }) => return Err(anyhow!("the bucket has no object at {object}")),
            Err(Unread::Unreachable(e) | Unread::Refused(e)) => return Err(e),
        };
        check_listing(listed, &bytes).with_context(|| format!("rebuilt from {}", self.bucket))?;
        Ok(bytes)
    }

    /// Whether the bucket's public origin answers `404` for the object of a range `listed` holds seqs of.
    /// Error cases:
    /// - an object's URL does not form, or the origin cannot be asked or answers other than `200` or `404`
    pub(crate) async fn lacks_a_range(&self, listed: &SegmentInfo) -> anyhow::Result<bool> {
        for index in listed.first_seq / RANGE_SEQS..=listed.last_seq / RANGE_SEQS {
            let url = self.bucket.object(index)?;
            let answer = self.http.head(url.clone()).send().await.with_context(|| format!("asking for {url}"))?;
            match answer.status() {
                StatusCode::OK => {}
                StatusCode::NOT_FOUND => return Ok(true),
                status => bail!("asking for {url}: the origin answered {status}"),
            }
        }
        Ok(false)
    }

    /// `listed`'s bytes from the bucket, or else from the first node origin that serves them as its listing names
    /// them; a node's redirect to the bucket, which was read first, is passed over.
    /// Error cases:
    /// - no source gives back the bytes the listing names: each source's failure
    pub(crate) async fn fetch(&self, listed: &SegmentInfo) -> anyhow::Result<Vec<u8>> {
        let mut failed = match self.read_back(listed).await {
            Ok(bytes) => return Ok(bytes),
            Err(e) => vec![format!("{e:#}")],
        };
        for origin in &self.nodes {
            match fetch_from(&self.http, origin, &listed.file, MAX_ARCHIVE_BYTES).await {
                Ok(Fetched::Bytes(bytes)) => match check_listing(listed, &bytes) {
                    Ok(()) => return Ok(bytes),
                    Err(e) => failed.push(format!("{origin}: {e:#}")),
                },
                Ok(Fetched::Moved(location)) => failed.push(format!("{origin}: redirected to {location}")),
                Err(e) => failed.push(format!("{origin}: {e:#}")),
            }
        }
        Err(anyhow!("no source gives back {}: {}", listed.file, failed.join("; ")))
    }
}

impl Window {
    /// The first seq of the log the floors keep on the host (`Floors`), counted from the newest completed segment's
    /// first command: none while either keeps every segment, or the newest completed segment cannot be read, which
    /// the removals then report.
    /// Error cases:
    /// - a listing, or the floors, cannot be read
    pub(super) async fn kept_from(
        &mut self,
        view: &Manifest,
        archived: u64,
        floors: &mut Option<Floors>,
    ) -> anyhow::Result<Option<u64>> {
        let Some(newest) = view.completed_len().checked_sub(1) else { return Ok(None) };
        let Ok(now) = self.stamp(&view.completed_at(newest)?).await else { return Ok(None) };
        let read = self.floors(archived, now, floors).await?;
        Ok(read.snapshot.zip(read.history).map(|(snapshot, history)| snapshot.min(history)))
    }

    /// Puts back each listed segment the host must keep and lacks, newest first, trying at most `MAX_RESTORES` a pass
    /// and none after one no source gives back: one after the oldest segment on the host, which a restart's read
    /// requires, and one before it, as long as the segment after it is on the host, so what is on the host stays one
    /// run, that the floors keep, holding a seq at or past `kept`, or that the uploader needs, holding a seq of a range
    /// at or past `archived` that the bucket lacks. Raises
    /// Critical `log_segment_missing`, naming the one no source gives back, the newest the host lacks and must keep,
    /// and resolves it once a pass leaves none lacking. Returns the files it put back.
    /// Error cases:
    /// - a listing cannot be read
    pub(super) async fn restore_lacking(
        &mut self,
        view: &Manifest,
        local: &mut BTreeSet<u64>,
        (archived, kept): (u64, Option<u64>),
    ) -> anyhow::Result<Vec<String>> {
        let first_local = local.first().copied().unwrap_or(u64::MAX);
        let from = first_local.min(archived).min(kept.unwrap_or(u64::MAX));
        let mut listed = Vec::new();
        if let Some((ordinal, _)) = view.containing(from)? {
            for ordinal in ordinal..view.completed_len() {
                listed.push(view.completed_at(ordinal)?);
            }
        }
        let (mut restored, mut tried, mut lacking) = (Vec::new(), 0, false);
        for (at, segment) in listed.iter().enumerate().rev() {
            if local.contains(&segment.first_seq) {
                continue;
            }
            let next_held = listed.get(at + 1).is_none_or(|next| local.contains(&next.first_seq));
            let needed = segment.first_seq >= first_local
                || (next_held && kept.is_some_and(|kept| segment.last_seq >= kept))
                || (segment.last_seq >= archived && next_held && self.unarchived(segment).await);
            if !needed {
                continue;
            }
            if tried >= MAX_RESTORES || lacking {
                // Left for the next pass, the alert as it stands.
                lacking = true;
                continue;
            }
            tried += 1;
            match self.put_back(view, segment).await {
                Ok(()) => {
                    local.insert(segment.first_seq);
                    restored.push(segment.file.clone());
                }
                Err(e) => {
                    lacking = true;
                    if self.raised.missing.replace(true) != Some(true) {
                        error!(
                            alert = "log_segment_missing",
                            "the log lists segment {}, seqs {} to {}, which this host must keep and lacks, and no \
                             source gives it back; the window tries the segments before it once it is back: {e:#}",
                            segment.file,
                            segment.first_seq,
                            segment.last_seq
                        );
                    } else {
                        warn!(segment = %segment.file, "no source gives back this segment the host lacks: {e:#}");
                    }
                }
            }
        }
        if !lacking && self.raised.missing.replace(false) != Some(false) {
            info!(alert_resolved = "log_segment_missing", "the host holds every segment of the log it must keep");
        }
        Ok(restored)
    }

    /// Whether the bucket lacks a range `segment` holds seqs of, so the uploader needs the segment: a count of the
    /// bucket's ranges a stale `404` at the uploader's resume lowered puts back nothing the bucket holds. A bucket
    /// that cannot be asked counts as holding it, as the uploads' lag reports.
    async fn unarchived(&self, segment: &SegmentInfo) -> bool {
        self.restore.lacks_a_range(segment).await.unwrap_or_else(|e| {
            debug!(segment = %segment.file, "the bucket could not be asked for this segment's ranges: {e:#}");
            false
        })
    }

    /// Reads `segment` from the archive and writes it into the log directory, `view`'s still.
    /// Error cases:
    /// - no source gives it back (`Sources::fetch`), or it cannot be written (`write`)
    /// - the directory is no longer the one `view` lists
    async fn put_back(&self, view: &Manifest, segment: &SegmentInfo) -> anyhow::Result<()> {
        let bytes = self.restore.fetch(segment).await?;
        ensure!(self.describes(view).await?, "the log's directory is no longer the one its writer lists");
        let (dir, listed) = (self.dir.clone(), segment.clone());
        spawn_blocking(move || write(&dir, &listed, &bytes)).await??;
        self.metrics.archive.window.restored.inc();
        warn!(segment = %segment.file, "the host lacked this segment of the log, which is put back from the archive");
        Ok(())
    }
}

/// Writes `bytes`, which are `listed`'s, into the log directory `dir` as its file: under a name of its own first,
/// created anew, synced, then renamed into place and the directory synced, so a crash leaves the segment whole or
/// absent, and a restore of the same segment beside it writes another file. A write that fails removes its file.
/// Error cases:
/// - `listed` is not named for its first seq (`validate_listing`), so its name could reach outside `dir`
/// - the file cannot be written, synced or renamed, or the directory synced
pub(crate) fn write(dir: &Path, listed: &SegmentInfo, bytes: &[u8]) -> anyhow::Result<()> {
    validate_listing(listed)?;
    let staged = staged(dir, &listed.file);
    let mut file = File::create_new(&staged).with_context(|| format!("creating {}", staged.display()))?;
    let written = (|| {
        file.write_all(bytes).with_context(|| format!("writing {}", staged.display()))?;
        file.sync_all().with_context(|| format!("syncing {}", staged.display()))?;
        std::fs::rename(&staged, dir.join(&listed.file)).with_context(|| format!("renaming {}", staged.display()))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written?;
    File::open(dir)?.sync_all().with_context(|| format!("syncing {}", dir.display()))
}

/// Removes the staged files a crash of an earlier process left in `dir` part way through a restore, leaving this
/// process's run's, which a restore of its own may be writing.
/// Error cases:
/// - the directory cannot be listed, or a staged file removed
pub(crate) fn remove_staged(dir: &Path) -> anyhow::Result<()> {
    let listing = match std::fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    for item in listing {
        let path = item.with_context(|| format!("listing {}", dir.display()))?.path();
        let mine = format!("{RESTORING}{:016x}.", *RUN);
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
        if name.contains(RESTORING) && !name.contains(&mine) {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

/// Puts back on the host every completed segment the manifest in the log directory `dir` lists that the host lacks,
/// from the archive `cfg`'s `[archive]` names, before a replay that reads the whole log: a log the local window
/// trimmed (`archive::window`), whose start goes on from no daily snapshot, or which rebuilds `[snapshots]`'s history
/// index and catalog. The manifest is the host's, as the whole replay reads it; that replay checks every segment as
/// it always does. Returns how many it put back; none without `[archive]`, which leaves the replay to refuse a log
/// whose oldest segments are absent (`log::NotLocal`).
/// Error cases:
/// - the manifest or the directory cannot be read, or a lacking segment cannot be read from the archive or written
pub(crate) async fn restore_whole(cfg: &NodeConfig, dir: &Path) -> anyhow::Result<u64> {
    let Some(sources) = Sources::for_config(cfg)? else { return Ok(0) };
    let read = dir.to_owned();
    let lacking = spawn_blocking(move || lacking(&read)).await??;
    if !lacking.is_empty() {
        warn!(
            segments = lacking.len(),
            "the whole log replays, and the host lacks segments the local window removed: they are read from the \
             archive and put back first"
        );
    }
    put_back_all(&sources, dir, &lacking).await.context("putting back a segment the whole replay reads")
}

/// Where a new or discarded history index takes the log in `cfg`'s log directory from (`fresh_start`), its writer's
/// `view` listing it: none while the host holds the log's first segment. When the index does not resume at or after
/// that start (`resumes`) and the host lacks the segments from there, as after `history.retention` grew past what the
/// window kept, they are put back from the archive first, so the index takes every command its retention holds from
/// the host; with no `[archive]` to put them back from, the index takes the log the host holds, with a Warning. A log
/// that cannot be read for it gives none, with a Warning, as the log's start.
/// Error cases:
/// - a lacking segment cannot be read from the archive or written: an index that took the log from the host's oldest
///   segment would hold less than the retention it serves
pub(crate) async fn history_start(
    cfg: &NodeConfig,
    view: &Manifest,
    resumes: impl Fn(u64) -> bool,
) -> anyhow::Result<Option<u64>> {
    let (dir, daily) = (cfg.node.log_dir.as_path(), cfg.daily_snapshots.as_ref().map(|daily| daily.dir.as_path()));
    let start = fresh_start(view, dir, daily, cfg.history.retention).unwrap_or_else(|e| {
        warn!("a new history index takes the log from its start, since the host's log cannot be read for it: {e:#}");
        None
    });
    let Some(FreshStart { from, first_local }) = start else { return Ok(None) };
    if from >= first_local || resumes(from) {
        return Ok(Some(from.max(first_local)));
    }
    let Some(sources) = Sources::for_config(cfg)? else {
        warn!(
            from,
            first_local,
            "the history index rebuilds from seq {from}, which the host no longer holds, and with no [archive] to put \
             it back from takes the log from seq {first_local}, the oldest the host holds"
        );
        return Ok(Some(first_local));
    };
    let mut lacking = Vec::new();
    if let (Some((first, _)), Some((local, _))) = (view.containing(from)?, view.containing(first_local)?) {
        for ordinal in first..local {
            lacking.push(view.completed_at(ordinal)?);
        }
    }
    warn!(
        from,
        segments = lacking.len(),
        "the history index rebuilds from seq {from}, which the host no longer holds: the segments are put back first"
    );
    put_back_all(&sources, dir, &lacking).await.context("putting back a segment the history index takes")?;
    Ok(Some(from))
}

/// Puts `lacking`, listings of the log in `dir` the host lacks, back from `sources`, newest first: an interruption
/// leaves the host's segments one run, which every read of the directory requires. What an earlier process's crash
/// left staged is removed first. How many it put back.
/// Error cases:
/// - a listing is not named for its first seq, or cannot be read from the archive or written
async fn put_back_all(sources: &Sources, dir: &Path, lacking: &[SegmentInfo]) -> anyhow::Result<u64> {
    if !lacking.is_empty() {
        let staged = dir.to_owned();
        spawn_blocking(move || remove_staged(&staged)).await??;
    }
    for listed in lacking.iter().rev() {
        let bytes = sources.fetch(listed).await?;
        let (into, listed, written) = (dir.to_owned(), listed.clone(), listed.file.clone());
        spawn_blocking(move || write(&into, &listed, &bytes)).await??;
        info!(segment = %written, "put back from the archive");
    }
    Ok(lacking.len() as u64)
}

/// The listings of the manifest in `dir` whose files are not there.
/// Error cases:
/// - the manifest or the directory cannot be read, or a listing is not named for its first seq
fn lacking(dir: &Path) -> anyhow::Result<Vec<SegmentInfo>> {
    let manifest = Manifest::load(dir).with_context(|| format!("reading {}", dir.join(MANIFEST).display()))?;
    let held: BTreeSet<u64> = list_segments(dir)?.into_iter().map(|(seq, _)| seq).collect();
    let lacking: Vec<SegmentInfo> =
        manifest.segments.iter().filter(|listed| !held.contains(&listed.first_seq)).cloned().collect();
    // The host's manifest names each file a restore writes: only a segment's own name is one.
    lacking.iter().try_for_each(validate_listing)?;
    Ok(lacking)
}

/// A name of its own `file` is written under before it is renamed into place.
pub(super) fn staged(dir: &Path, file: &str) -> PathBuf {
    let n = STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    dir.join(format!("{file}{RESTORING}{:016x}.{n}", *RUN))
}
