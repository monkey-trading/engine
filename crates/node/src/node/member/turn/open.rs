//! A member's copy at its turn: opening it and its replica, the incumbent a stale copy catches up
//! from, and the tail handover that takes the turn.

use std::collections::BTreeSet;

use engine_core::eip712::B256;

use crate::checkpoints::Observation;
use crate::daily_snapshots::{AcceptedRoots, Fetching, SnapshotBucket, VaultRoots};
use crate::handover::Requester;
use crate::sequencer::replay::RETAINED_LINKS;
use crate::standby::archive::archive_client;

use super::*;

/// How many turns back the start of the in-office member's run of turns is read: one that held more is treated as
/// holding every turn, so no snapshot is taken, since the member's own log check would not count.
const RUN_TURNS: usize = 32;

impl<C: CheckpointChain, H: PostedHead, T: Sampling> Member<'_, C, H, T> {
    /// Opens the copy, bootstrapping an empty one from `operator`'s archive when given, and builds its
    /// replica from the newest daily snapshot it can start from (`snapshot_start`), or else by one replay,
    /// or two when it cuts an unaccepted tail back (`Follower::replays`), authenticated against every signer
    /// the vault recorded, with the dashboard record and the history index's check beside it, and the latest
    /// posted head pinned.
    /// Error cases:
    /// - the archive, the vault, the history index or the copy cannot be read, or, for a copy that holds nothing yet,
    ///   the schedule or trusted time under `[daily_snapshots]`, or the copy does not replay (`CopyRefused`)
    pub(in crate::node::member) async fn open(&self, operator: Option<&Followed>) -> anyhow::Result<Opened> {
        self.open_admitting(operator, None, &[]).await
    }

    /// `open`, with `admitted` among the signers the copy replays under: the key a standing promotion
    /// made the vault's signer, whose seals and checkpoints the copy holds before finality names it;
    /// the links and checkpoints at `keep` are kept past retention, and the replica starts from no snapshot
    /// at or after the first of them.
    /// Error cases:
    /// - as `open`
    pub(in crate::node::member) async fn open_admitting(
        &self,
        operator: Option<&Followed>,
        admitted: Option<AccountId>,
        keep: &[u64],
    ) -> anyhow::Result<Opened> {
        let dir = self.cfg.node.log_dir.as_path();
        // The archive's uploader and the log's local window follow no writer while the copy opens, as again while
        // a replica it replaces still runs: the window would remove segments the open puts back or reads.
        if let Some(archive) = self.delivered.and_then(|delivered| delivered.archive.as_ref()) {
            archive.send_replace(None);
        }
        // The chain and trusted time are read before the copy is bootstrapped: a read that fails then leaves the
        // copy empty, so the open tried again bootstraps it again, and may still start its replica from a snapshot.
        let at = self.vault.observe(View::Finalized, &[]).await?;
        let mut signers = crate::handover::vault_signers(&self.vault, &at).await?;
        let final_epoch = at.lifecycle.as_ref().map_or(1, |lifecycle| lifecycle.signer_epoch);
        signers.extend(Box::pin(crate::node::member::standby::named_since(self.cfg, final_epoch)).await?);
        let enclave = operator.map_or(at.state.signer, |operator| operator.enclave);
        signers.insert(enclave);
        signers.extend(admitted);
        let domain = CheckpointDomain { chain_id: self.cfg.custody.chain_id, vault: self.cfg.custody.vault.parse()? };
        // A copy that holds the log builds its replica whole when the bound cannot be read, rather than not at all.
        let bound = async {
            let now_ms = self.time.sample().await?.earliest_ms;
            in_office_since(&self.vault, &at, now_ms, self.release.id()).await
        };
        let before = match self.cfg.daily_snapshots {
            Some(_) => match bound.await {
                Ok(before) => Some(before),
                Err(e) if dir.join(MANIFEST).exists() => {
                    warn!(alert = "snapshot_start_refused", "the replica replays the whole copy: {e:#}");
                    None
                }
                Err(e) => return Err(e),
            },
            None => Some(None),
        };
        // The posted head is pinned as the replay passes it, so a rebuilt replica still holds it.
        let posted = match &self.heads {
            Some(heads) => heads.posted_head().await.ok().flatten().map(|head| (head.seq, head.link)),
            None => None,
        };
        let archive = operator.map(|operator| archive_client(operator.danger_accept_invalid_certs)).transpose()?;
        let origins = operator.map(Followed::origins).unwrap_or_default();
        if let Some(archive) = archive.as_ref().filter(|_| !dir.join(MANIFEST).exists()) {
            let backfilled = operator.map(Followed::archive).unwrap_or_default();
            crate::standby::archive::bootstrap_archive(archive, &backfilled, dir).await?;
        }
        let roots = VaultRoots { vault: &self.vault, at: &at };
        let bucket = SnapshotBucket::for_config(self.cfg)?;
        let fetching = archive.as_ref().map(|client| Fetching { client, origins: &origins, bucket: bucket.as_ref() });
        let finalized = at.state.checkpoint.as_ref().map(|(seq, _)| *seq);
        let (stats, mut history) = (Stats::new(self.cfg)?, HistoryPlan::for_member(&self.cfg.history)?);
        // A snapshot at or before the history index's last command, where this member's marks cache ended too, so the
        // replica passes it and folds every mark after it: one past it would leave both a gap the start cannot fill.
        let held_to = history.last_seq().map(|last| last.saturating_add(1));
        let keep_from = keep_from(keep, posted).into_iter().chain(held_to).min();
        let snapshot = match before {
            Some(before) => {
                self.snapshot_start(fetching, (finalized, before, keep_from), (domain, &signers), &roots).await
            }
            None => None,
        };
        let snapshot_seq = snapshot.as_ref().map(|signed| signed.snapshot.offset.seq);
        // The marks the replica applies, folded for the cache this member cannot write while it follows.
        let marks = Rc::new(RefCell::new(MarkFold::new(Retentions::of(self.cfg.history.marks.as_ref())?)));
        let started = Instant::now();
        let segment_commands = self.cfg.node.segment_commands;
        let daily = self.cfg.daily_snapshots.as_ref().map(|daily| daily.dir.clone());
        let rows =
            Arc::new(RowsStore::new(&self.cfg.history.dir, self.cfg.history.retention_ms()?).with_snapshots(daily));
        let kept = (Some(stats.clone()), Some(marks.clone()), Some(self.checked.clone()), Some(rows));
        let stopping = &self.stopping;
        let pins = (posted, keep);
        let workers = self.cfg.node.replay_workers;
        let observe = |entry: &crate::log::LogEntry| {
            anyhow::ensure!(!stopping.load(Ordering::Acquire), Stopping);
            history.observe(entry);
            Ok(())
        };
        // A replica built from the whole copy reads the segments the local window removed, which the archive holds.
        if snapshot.is_none() {
            crate::archive::restore_whole(self.cfg, dir).await?;
        }
        let opened = Follower::open_pinned(
            dir,
            segment_commands,
            workers,
            domain,
            enclave,
            signers,
            kept,
            pins,
            snapshot,
            observe,
        );
        let follower = match opened {
            Ok(follower) => follower,
            // A whole replay, as a snapshot the replica refused gives way to, read a segment the window removed, or
            // one a pass still under way removed after the restore above, which leaves a hole inside the run that the
            // replay reports as a plain error, not `NotLocal`: once the segments are back from the archive, the open
            // runs again from its start, the window following no writer now. A stop is no copy to restore.
            Err(e) if !e.is::<Stopping>() => {
                if crate::archive::restore_whole(self.cfg, dir).await? == 0 {
                    return Err(e.context(CopyRefused));
                }
                return Box::pin(self.open_admitting(operator, admitted, keep)).await;
            }
            Err(e) => return Err(e.context(CopyRefused)),
        };
        // A replica from a snapshot observed none of the copy before it, where the history index may have stopped.
        if let Some(seq) = snapshot_seq {
            history.observe_before(dir, seq);
        }
        // An index that rebuilds at this member's turn takes the log from where it re-derives, which the copy lacks
        // after `history.retention` grew past what the window kept: it is put back now, not at the turn, whose start
        // tries again and refuses while the archive cannot give it back.
        let view = follower.manifest().borrow().clone();
        if let Err(e) = crate::archive::history_start(self.cfg, &view, |from| history.resumes_from(from)).await {
            warn!("the log a rebuilt history index takes is not back on the host before the turn: {e:#}");
        }
        self.metrics.full_replays.inc_by(follower.replays());
        if let Some(archive) = self.delivered.and_then(|delivered| delivered.archive.as_ref()) {
            archive.send_replace(Some(follower.manifest()));
        }
        info!(seq = ?follower.replica().last_seq(), elapsed = ?started.elapsed(), "built the replica from the copy");
        Ok((follower, stats, history, marks))
    }

    /// The daily snapshot the replica of the copy starts from, under `[daily_snapshots]`: the newest, of those the
    /// directory holds and those the origins list, when `fetching` names them, fetched with its client from their
    /// disk or the bucket a `308` names, the one `[archive]` names where it names one, whose checkpoint the copy
    /// holds and the vault has finalized, at `finalized`, from before the turns of the member in office began, at
    /// `before` (`in_office_since`), and before `keep_from`, signed over its state by the
    /// recorded signer of that checkpoint, one of `signers`, and of the root the vault accepted at its seq where
    /// `roots` names one (`daily_snapshots::snapshot_start_against`). None when the section is absent, the vault
    /// has finalized no checkpoint or the copy holds none, or no snapshot passes; a failure is paged as Warning
    /// `snapshot_start_refused`, and the replica replays the whole copy.
    async fn snapshot_start(
        &self,
        fetching: Option<Fetching<'_>>,
        (finalized, before, keep_from): (Option<u64>, Option<Millis>, Option<u64>),
        (domain, signers): (CheckpointDomain, &BTreeSet<AccountId>),
        roots: &impl AcceptedRoots,
    ) -> Option<crate::daily_snapshots::Signed> {
        let snapshots = self.cfg.daily_snapshots.as_ref()?.dir.as_path();
        let dir = self.cfg.node.log_dir.as_path();
        // An archive that publishes nothing yet, as a fresh deployment's, leaves the copy empty.
        if !dir.join(MANIFEST).exists() {
            return None;
        }
        let copied = crate::log::last_checkpoint(dir)
            .inspect_err(|e| {
                warn!(alert = "snapshot_start_refused", "the copy's last checkpoint cannot be read: {e:#}")
            })
            .ok()??;
        let bounds = crate::daily_snapshots::Bounds { finalized: finalized?.min(copied.seq), before, keep_from };
        crate::daily_snapshots::snapshot_start_against(fetching, (snapshots, dir), (domain, signers), bounds, roots)
            .await
    }

    /// Takes `turn` from the first of its sources that hands the keys over with the tail past the
    /// follower's tip, caught up once more from `operator`, the incumbent it followed, if any, after
    /// the start's reads, and following it while each source stops, its request signed with the
    /// standby key of `standby`, this member's standby side, when open, or else with this release's own
    /// (`member::requester`), with the admission naming that key, if any. Everything the start reads from
    /// the chain is read before connecting, and the trusted time sample that admits its first stamps is
    /// taken last (`Ready`): from the confirmation on, nothing reads the chain or samples time.
    /// Error cases:
    /// - the reads or the sample fail, no source has a handover endpoint, or every source fails
    pub(super) async fn take(
        &self,
        turn: &ScheduledTurn,
        follower: &mut Follower,
        operator: Option<&Followed>,
        standby: Option<&Standby>,
    ) -> anyhow::Result<(Taken, Ready)> {
        let (sources, mut prepared) = self.prepare_take(turn).await?;
        // The standby side's key while it is open, as the registration it may have names; this release's own
        // sealed one otherwise, as a member with no standby side holds.
        let sealing = self.cfg.tee.sealing.as_ref().map(|sealing| sealing.dir.as_path());
        prepared.requester = match (standby, sealing) {
            (Some(open), Some(dir)) => Some(Arc::new(Requester::with_admission(open.keys.clone(), dir))),
            _ => crate::node::member::requester(self.cfg, &self.start)?.map(Arc::new),
        };
        // The reassignments the start's own log check counts, read with the rest: none after the confirmation.
        // The keys bring the last verified head only then, so the read cannot start at the range's in-office turn.
        let own = [turn.candidate].into();
        let reassigned = crate::node::log_check::read_reassigned(&self.vault, (turn.turn, None), None, &own).await?;
        let read = Ready::read(
            self.cfg,
            &self.start,
            &self.metrics,
            prepared.signer,
            follower.replica(),
            (turn, prepared.generation),
            self.time.sample(),
        );
        let mut ready = read.await?;
        ready.reassigned = Some(reassigned);
        prepared.posted = ready.heads.and_then(|heads| heads.newest).map(|(_, head)| head);
        // The reads took time: following the incumbent once more catches the copy up and keeps the tail
        // short, for `CATCH_UP` in all, connecting included, so an incumbent that stalls its handshake
        // delays the take by no more than that. A replica that refuses an entry must be rebuilt; any
        // other failure leaves the tail to cover the gap.
        if let Some(operator) = operator {
            let pause = tokio::time::sleep(CATCH_UP);
            match follower.follow_during(self.cfg, operator, CATCH_UP, pause).await.1 {
                Ok(()) => {}
                Err(e) if e.downcast_ref::<ReplicaRefused>().is_some() => return Err(e),
                Err(e) => warn!("catching the copy up before the tail failed: {e:#}"),
            }
        }
        // Every source's error is paged and classified as it comes, so an incumbent's refusal, or a
        // copy its tail changed, is not lost behind a later source's failure.
        let (mut failures, mut failed) = (Vec::new(), SourcesFailed::default());
        for source in sources {
            info!(turn = turn.turn, predecessor = %source.handover_endpoint, "taking this member's turn with the tail");
            let (sealer, attestor) = (self.sealer.as_ref(), self.attestor.as_ref());
            let endpoint = &source.handover_endpoint;
            let quoting = (attestor, &self.collateral);
            match take_tail(self.cfg, endpoint, sealer, quoting, &prepared, (follower, operator)).await {
                Ok(taken) => return Ok((taken, ready)),
                Err(e) => {
                    page_refusal(turn, &e);
                    failed.record(&e);
                    failures.push(format!("{endpoint}: {e:#}"));
                }
            }
        }
        let context = format!("no operator handed turn {}'s keys over: {}", turn.turn, failures.join("; "));
        Err(anyhow::Error::new(failed).context(context))
    }

    /// The operator `turn` takes over from, at `https://` the hostname its candidacy registered,
    /// against the vault's signer at finality: whom a stale copy catches up from.
    /// Error cases:
    /// - the turn names no source, or the vault cannot be read
    pub(super) async fn incumbent(&self, turn: &ScheduledTurn) -> anyhow::Result<Followed> {
        let source = turn.source.as_ref().context("the turn names no source to follow")?;
        let at = self.vault.observe(View::Finalized, &[]).await?;
        let handover = self.cfg.tee.sealing.as_ref().and_then(|sealing| sealing.handover.as_ref());
        Ok(Followed {
            origin: format!("https://{}", source.api_hostname),
            enclave: at.state.signer,
            mirrors: handover.map(|handover| handover.mirrors.clone()).unwrap_or_default(),
            danger_accept_invalid_certs: handover.is_some_and(|handover| handover.danger_accept_invalid_certs),
            bucket: crate::archive::Bucket::for_config(self.cfg)?,
        })
    }
}

/// The first seq a replica's snapshot must precede: the first of `keep`, whose links and checkpoints the replica
/// keeps as it applies them, and the seq past which the links a snapshot carries no longer reach back to
/// `posted`, the posted head the replica pins (`LiveReplay::resume`), which a start from it locates.
pub(super) fn keep_from(keep: &[u64], posted: Option<(u64, B256)>) -> Option<u64> {
    let reach = posted.map(|(seq, _)| seq.saturating_add(RETAINED_LINKS as u64));
    keep.iter().copied().chain(reach).min()
}

/// When the turns of the member in office at `now_ms` began, as the finalized schedule at `at` draws them: the
/// boundary of the first of the consecutive turns its candidate holds, read back at most `RUN_TURNS` turns, and the
/// earliest time for a longer run; none while no turn is in effect. A turn drawn for `own`, this member's release,
/// which it has yet to take, is passed over for the one before it. A member whose replica starts from a snapshot
/// stamped before it has replayed every command of those turns, the range the verified head stands before as the
/// keys reach the member at its first take (`log_check::own_check`): the check a member does at that take counts
/// only for commands its replica applied. `now_ms` picks only that bound: a wrong one makes that check wait for a
/// later one, never count where it should not.
/// Error cases:
/// - the schedule cannot be read
pub(in crate::node::member) async fn in_office_since(
    vault: &impl CheckpointChain,
    at: &Observation,
    now_ms: Millis,
    own: [u8; 32],
) -> anyhow::Result<Option<Millis>> {
    // The turn drawn before the one that begins at `boundary`.
    let before = async |boundary: Millis| {
        let read = vault.scheduled_turn(at, boundary.saturating_sub(1)).await.context("reading the operator schedule");
        Ok::<_, anyhow::Error>(read?.filter(|turn| turn.boundary < boundary))
    };
    let Some(mut current) = crate::handover::turns::turn_at(vault, at, Some(now_ms)).await? else { return Ok(None) };
    let mut first = current.boundary;
    for _ in 0..RUN_TURNS {
        match before(first).await? {
            Some(earlier) if current.release == own => {
                first = earlier.boundary;
                current = earlier;
            }
            Some(earlier) if earlier.candidate == current.candidate => first = earlier.boundary,
            Some(_) => return Ok(Some(first)),
            None if current.release == own => return Ok(None),
            None => return Ok(Some(first)),
        }
    }
    Ok(Some(Millis::MIN))
}
