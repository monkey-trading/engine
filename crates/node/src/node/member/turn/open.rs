//! A member's copy at its turn: opening it and its replica, the incumbent a stale copy catches up
//! from, and the tail handover that takes the turn.

use std::collections::BTreeSet;

use super::*;

impl<C: CheckpointChain, H: PostedHead, T: Sampling> Member<'_, C, H, T> {
    /// Opens the copy, bootstrapping an empty one from `operator`'s archive when given, and builds its
    /// replica by one replay, or two when it cuts an unaccepted tail back (`Follower::replays`), or, for a
    /// copy it bootstrapped, from the newest daily snapshot it can start from (`snapshot_start`),
    /// authenticated against every signer the vault recorded, with the dashboard record and the history
    /// index's check beside it, and the latest posted head pinned.
    /// Error cases:
    /// - the archive, the vault, the history index or the copy cannot be read, or the copy does not
    ///   replay (`CopyRefused`)
    pub(in crate::node::member) async fn open(&self, operator: Option<&Followed>) -> anyhow::Result<Opened> {
        self.open_admitting(operator, None, &[]).await
    }

    /// `open`, with `admitted` among the signers the copy replays under: the key a standing promotion
    /// made the vault's signer, whose seals and checkpoints the copy holds before finality names it;
    /// the links and checkpoints at `keep` are kept past retention.
    /// Error cases:
    /// - as `open`
    pub(in crate::node::member) async fn open_admitting(
        &self,
        operator: Option<&Followed>,
        admitted: Option<AccountId>,
        keep: &[u64],
    ) -> anyhow::Result<Opened> {
        let dir = self.cfg.node.log_dir.as_path();
        // The chain is read before the copy is bootstrapped: a read that fails then leaves the copy empty, so the
        // open tried again bootstraps it again, and may still start its replica from a snapshot.
        let at = self.vault.observe(View::Finalized, &[]).await?;
        let mut signers = crate::handover::vault_signers(&self.vault, &at).await?;
        let final_epoch = at.lifecycle.as_ref().map_or(1, |lifecycle| lifecycle.signer_epoch);
        signers.extend(Box::pin(crate::node::member::standby::named_since(self.cfg, final_epoch)).await?);
        let enclave = operator.map_or(at.state.signer, |operator| operator.enclave);
        signers.insert(enclave);
        signers.extend(admitted);
        let domain = CheckpointDomain { chain_id: self.cfg.custody.chain_id, vault: self.cfg.custody.vault.parse()? };
        let mut snapshot = None;
        if let Some(operator) = operator.filter(|_| !dir.join(MANIFEST).exists()) {
            let archive = crate::standby::archive::archive_client(operator.danger_accept_invalid_certs)?;
            crate::standby::archive::bootstrap_archive(&archive, &operator.origins(), dir).await?;
            let finalized = at.state.checkpoint.as_ref().map(|(seq, _)| *seq);
            snapshot = self.snapshot_start((archive, operator.origins()), finalized, domain, &signers).await;
        }
        let (stats, mut history) = (Stats::new(self.cfg)?, HistoryPlan::for_member(&self.cfg.history)?);
        // The posted head is pinned as the replay passes it, so a rebuilt replica still holds it.
        let posted = match &self.heads {
            Some(heads) => heads.posted_head().await.ok().flatten().map(|head| (head.seq, head.link)),
            None => None,
        };
        // The marks the replica applies, folded for the cache this member cannot write while it follows.
        let marks = Rc::new(RefCell::new(MarkFold::new(Retentions::of(self.cfg.history.marks.as_ref())?)));
        let started = Instant::now();
        let segment_commands = self.cfg.node.segment_commands;
        let rows = Arc::new(RowsStore::new(&self.cfg.history.dir, self.cfg.history.retention_ms()?));
        let kept = (Some(stats.clone()), Some(marks.clone()), Some(self.checked.clone()), Some(rows));
        let stopping = &self.stopping;
        let pins = (posted, keep);
        let workers = self.cfg.node.replay_workers;
        let observe = |entry: &crate::log::LogEntry| {
            anyhow::ensure!(!stopping.load(Ordering::Acquire), Stopping);
            history.observe(entry);
            Ok(())
        };
        let follower = Follower::open_pinned(
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
        )
        .map_err(|e| e.context(CopyRefused))?;
        self.metrics.full_replays.inc_by(follower.replays());
        info!(seq = ?follower.replica().last_seq(), elapsed = ?started.elapsed(), "built the replica from the copy");
        Ok((follower, stats, history, marks))
    }

    /// The daily snapshot the replica of a copy just bootstrapped from the archives of `origins` starts from,
    /// under `[daily_snapshots]`: the newest, of those the directory holds and those the origins list, fetched
    /// with `archive`, whose checkpoint the copy holds and the vault has finalized, at `finalized`, signed over
    /// its state by the recorded signer of that checkpoint, one of `signers` (`daily_snapshots::snapshot_start`).
    /// None when the section is absent, the vault has finalized no checkpoint or the copy holds none, or no
    /// snapshot passes; a failure is paged as Warning `snapshot_start_refused`, and the replica replays the
    /// whole copy.
    async fn snapshot_start(
        &self,
        (archive, origins): (reqwest::Client, Vec<String>),
        finalized: Option<u64>,
        domain: CheckpointDomain,
        signers: &BTreeSet<AccountId>,
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
        let bound = finalized?.min(copied.seq);
        crate::daily_snapshots::snapshot_start(&archive, &origins, (snapshots, dir), domain, signers, bound).await
    }

    /// Takes `turn` from the first of its sources that hands the keys over with the tail past the
    /// follower's tip, caught up once more from `operator`, the incumbent it followed, if any, after
    /// the start's reads, and following it while each source stops. Everything the start reads from the chain is read before connecting, and the
    /// trusted time sample that admits its first stamps is taken last (`Ready`): from the confirmation
    /// on, nothing reads the chain or samples time.
    /// Error cases:
    /// - the reads or the sample fail, no source has a handover endpoint, or every source fails
    pub(super) async fn take(
        &self,
        turn: &ScheduledTurn,
        follower: &mut Follower,
        operator: Option<&Followed>,
    ) -> anyhow::Result<(Taken, Ready)> {
        let (sources, mut prepared) = self.prepare_take(turn).await?;
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
            match take_tail(self.cfg, endpoint, sealer, attestor, &prepared, (follower, operator)).await {
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
        })
    }
}
