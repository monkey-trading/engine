//! Where a replay that derives a command's rows again starts: from the newest daily snapshot the node holds that
//! is early enough, its engine at the snapshot's checkpoint, or from genesis, with an empty engine, when none is.
//! The replay reads only the log after where it starts, so the segments before that need not be on this host. Only
//! a snapshot whose file the host holds is taken: one the archive's bucket keeps instead is older than the host's
//! window, whose commands it keeps a snapshot before (`daily_snapshots::upload`). A snapshot's file is the host's,
//! of any format: each command's logged events must be the ones it gives, among
//! them each rollover's `financing_applied` and its root over the rows, against which a read checks the rows.

use std::path::Path;

use anyhow::{Context, ensure};
use engine_core::Engine;
use engine_core::event::Event;
use tracing::{info, warn};

use crate::daily_snapshots::{self, Listed};
use crate::log::CommandEntry;

/// How many of the newest snapshots that are early enough a replay tries before it starts from genesis.
const ATTEMPTS: usize = 3;

/// The engine a replay starts from: at the checkpoint of the newest snapshot listed in `snapshots` that `fits`,
/// or empty, before genesis, when there is none or none of the newest `ATTEMPTS` loads, each that does not
/// warned of.
pub(crate) fn engine_from(snapshots: Option<&Path>, fits: impl Fn(&Listed) -> bool) -> Engine {
    let Some(dir) = snapshots else { return Engine::new() };
    for listing in newest(dir, fits) {
        match daily_snapshots::load(&dir.join(&listing.file)) {
            Ok(snapshot) => {
                info!(seq = listing.seq, "re-deriving rows from the daily snapshot at seq {}", listing.seq);
                return snapshot.engine;
            }
            Err(e) => warn!(seq = listing.seq, "a daily snapshot does not load, so an earlier one is tried: {e:#}"),
        }
    }
    Engine::new()
}

/// The newest `ATTEMPTS` snapshots listed in `dir` that `fits` and whose file the host holds, newest first; none when
/// the listing cannot be read.
pub(crate) fn newest(dir: &Path, fits: impl Fn(&Listed) -> bool) -> Vec<Listed> {
    match daily_snapshots::read_manifest(dir) {
        Ok(listed) => listed
            .into_iter()
            .rev()
            .filter(|listing| fits(listing) && dir.join(&listing.file).exists())
            .take(ATTEMPTS)
            .collect(),
        Err(e) => {
            warn!("the daily snapshots cannot be listed, so rows are derived from genesis: {e:#}");
            Vec::new()
        }
    }
}

/// Whether a replay whose engine stands at `at`, none before genesis, goes on to the command at `seq`: it is
/// before it, and the newest snapshot in `snapshots` before it, if any, is not past the engine.
pub(crate) fn goes_on(at: Option<u64>, seq: u64, snapshots: Option<&Path>) -> bool {
    let newest = snapshots.and_then(|dir| newest(dir, |listing| listing.seq < seq).into_iter().next());
    at < Some(seq) && newest.is_none_or(|listing| Some(listing.seq) <= at)
}

/// Applies `logged`, a command the log holds, to `engine`, checking the events it gives against those it logs.
/// Error cases:
/// - the command does not apply, or its logged events differ from the engine's
pub(crate) fn apply_logged(engine: &mut Engine, logged: &CommandEntry) -> anyhow::Result<Vec<Event>> {
    let at = logged.sequenced.seq;
    let events = engine.apply(&logged.sequenced).with_context(|| format!("re-deriving at seq {at}"))?;
    // No root is computed, the only thing that drains the engine's record of changed keys, so it is dropped
    // here or it grows with every order the log ever held.
    engine.discard_changed_keys();
    ensure!(
        logged.events.as_ref().is_none_or(|logged| *logged == events),
        "events logged at seq {at} differ from the events replay gives"
    );
    Ok(events)
}
