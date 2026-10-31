//! Pure recovery validation and restoration of authenticated durable checkpoint work.

use super::*;
use crate::sequencer::replay::Replay;

impl ReplayedLog {
    /// Reports whether actual recovery used a cache, for integrated startup assertions.
    #[cfg(test)]
    pub(crate) fn snapshot_sequence(&self) -> Option<u64> {
        self.snapshot_seq
    }

    /// The last checkpoint the replay verified: the snapshot's it started at, or a later entry's.
    pub(crate) fn last_checkpoint(&self) -> Option<&crate::log::Checkpoint> {
        self.replayed.last_signed_checkpoint.as_ref()
    }

    /// Last engine command authenticated by the read-only replay.
    pub(crate) fn last_seq(&self) -> Option<u64> {
        self.replayed.engine.last_seq()
    }

    /// The emission schedule and progress the replayed log records, if it initialised emissions.
    pub(crate) fn emissions(&self) -> Option<&engine_core::emissions::Emissions> {
        self.replayed.engine.emissions()
    }

    /// Requires a live reserved signature to occur in the same authenticated durable tail.
    /// Error cases:
    /// - a reserved checkpoint conflicts with the anchor or is absent from outstanding work
    pub(crate) fn validate_submission(
        &self,
        parent: crate::log::CheckpointParent,
        reserved: Option<&crate::log::Checkpoint>,
    ) -> anyhow::Result<()> {
        let Some(reserved) = reserved else { return Ok(()) };
        if parent.has_checkpoint && reserved.seq <= parent.seq {
            anyhow::ensure!(
                reserved.seq < parent.seq || reserved.root == parent.root.to_string(),
                "reserved checkpoint conflicts with finalized recovery anchor"
            );
            return Ok(());
        }
        let found = self
            .replayed
            .recovery_work
            .iter()
            .find(|work| work.checkpoint.seq == reserved.seq)
            .context("reserved outstanding checkpoint is absent from the authenticated recovery log")?;
        anyhow::ensure!(
            serde_json::to_value(&found.checkpoint)? == serde_json::to_value(reserved)?,
            "reserved outstanding checkpoint differs from authenticated recovery work"
        );
        Ok(())
    }

    /// Checks every pure recovery refusal before history repair or opening the log for append.
    /// Error cases:
    /// - the deployment, genesis, or required checkpoint service differs from the replayed log
    /// - any restored checkpoint has no provable custody prefix
    pub(crate) fn validate_recovery(&self, cfg: &SequencerConfig<'_>, signer: AccountId) -> anyhow::Result<()> {
        let Replayed { engine, genesis, deferred, requires_checkpoint_gate, legacy_deposits, legacy_approvals, .. } =
            &self.replayed;
        if ProtocolVersion::LATEST.supports_deposit_finality()
            && !legacy_approvals.all_authorized(&{
                let mut authorized =
                    legacy_approvals.authorize(engine, legacy_deposits, &legacy::FinalityProof::default())?;
                authorized.extend(self.legacy_authorizations.iter().map(|(key, seq)| (*key, *seq)));
                authorized
            })
        {
            bail!("legacy withdrawal needs canonical finality proof before writable recovery");
        }
        // A config naming another client auth or operator, and so another chain, vault, or admin key,
        // is the wrong one for this log: refuse before anything is written or signed under it.
        if let Some(recorded) = engine.client_auth().filter(|recorded| **recorded != cfg.client_auth) {
            bail!("the log records client auth {recorded:?}, but the config gives {:?}", cfg.client_auth);
        }
        // A rotation the log records changes the operator's key only, never its chain or vault.
        let rotated = engine.operator_bounds().rotations > 0;
        let same_domain =
            |recorded: &Operator| (recorded.chain_id, recorded.vault) == (cfg.operator.chain_id, cfg.operator.vault);
        if let Some(recorded) =
            engine.operator().filter(|recorded| **recorded != cfg.operator && !(rotated && same_domain(recorded)))
        {
            bail!("the log records operator {recorded:?}, but the config gives {:?}", cfg.operator);
        }
        if let Some(recorded) = engine.access_policy()
            && cfg.access_policy.as_ref() != Some(recorded)
        {
            bail!("the configured access policy differs from the public log");
        }
        genesis.ensure_closed()?;
        // So is one naming another genesis: the node would attest rules its engine does not run.
        genesis.ensure_matches(&cfg.genesis, &cfg.client_auth, cfg.operator)?;
        if (*requires_checkpoint_gate || !deferred.is_empty()) && cfg.checkpoints.is_none() {
            bail!("checkpoint-gated withdrawals require the checkpoint service");
        }
        if cfg.checkpoints.is_some() && cfg.checkpoint_parent.has_checkpoint {
            let work = self.replayed.recovery_anchor_work.as_ref().filter(|work| {
                work.checkpoint.seq == cfg.checkpoint_parent.seq
                    && work.checkpoint.root == cfg.checkpoint_parent.root.to_string()
            });
            work.context("finalized recovery anchor lacks authenticated accounting")?;
        }
        if cfg.checkpoints.is_some() {
            for work in self.replayed.recovery_anchor_work.iter().chain(&self.replayed.recovery_work) {
                work.validate_recovery_prefix()?;
            }
        }
        // Last, so a log refused for its deployment or genesis says so.
        self.replayed
            .origin
            .require_recovery(crate::log::CheckpointDomain { chain_id: cfg.chain_id, vault: cfg.vault }, signer)
    }
}

impl Sequencer {
    /// Rebuilds state by replaying the existing log, if there is one, and opens it for appending.
    /// Signs, logs, and hands to custody any withdrawal the log ends before signing.
    ///
    /// The node checks the posted chain anchor before invoking recovery.
    /// Error cases:
    /// - the log cannot be read or replayed (see `read_log`)
    /// - the log records a client auth or an operator other than the config's, or a genesis it did
    ///   not apply in full, one no config loaded today can give, or one other than the config's
    /// - the log cannot be opened for appending, or written
    pub fn recover(
        cfg: SequencerConfig<'_>,
        key: Arc<EnclaveKey>,
        stream: broadcast::Sender<Arc<StreamMsg>>,
        withdrawals: mpsc::UnboundedSender<SignedWithdrawal>,
    ) -> anyhow::Result<Self> {
        let log = Self::read_log_for_recovery(
            cfg.log_dir,
            Some(crate::log::CheckpointDomain { chain_id: cfg.chain_id, vault: cfg.vault }),
            Some(AccountId(key.address())),
            cfg.checkpoints.as_ref().map(|_| cfg.checkpoint_parent),
            |_| {},
        )?;
        Self::recover_from(log, cfg, key, stream, withdrawals)
    }

    /// Reads the log in `dir` a segment at a time and replays it as it goes, handing each entry to
    /// `observe` as well, so that a check of the log before recovery sees exactly what it replays.
    /// Error cases:
    /// - the log cannot be read, or fails its manifest check (see `SegmentedLog::open`) or replay
    ///   (see `Replay`)
    pub fn read_log(dir: &Path, mut observe: impl FnMut(&LogEntry)) -> anyhow::Result<ReplayedLog> {
        Self::read_log_in_domain(dir, None, &mut observe)
    }

    /// Reads one log in a known custody deployment; signatures and the observer see that read.
    /// Error cases:
    /// - log reading or strict signature/root replay fails
    pub fn read_log_in_domain(
        dir: &Path,
        domain: Option<crate::log::CheckpointDomain>,
        mut observe: impl FnMut(&LogEntry),
    ) -> anyhow::Result<ReplayedLog> {
        Self::read_log_with_trust(dir, domain, None, &mut observe)
    }

    /// Reads one authenticated stream with an independently trusted withdrawal authority.
    /// Error cases:
    /// - reading, replay, or release authorization fails
    pub fn read_log_with_trust(
        dir: &Path,
        domain: Option<crate::log::CheckpointDomain>,
        signer: Option<AccountId>,
        mut observe: impl FnMut(&LogEntry),
    ) -> anyhow::Result<ReplayedLog> {
        Self::read_log_for_recovery(dir, domain, signer, None, &mut observe)
    }

    /// Replays the exact signed outstanding chain relative to a trusted finalized anchor.
    /// Error cases:
    /// - reading, signature/root replay, or outstanding parent continuity fails
    pub(crate) fn read_log_for_recovery(
        dir: &Path,
        domain: Option<crate::log::CheckpointDomain>,
        signer: Option<AccountId>,
        parent: Option<crate::log::CheckpointParent>,
        mut observe: impl FnMut(&LogEntry),
    ) -> anyhow::Result<ReplayedLog> {
        Self::read_log_with_authorities(dir, domain, signer, parent, Default::default(), &mut observe, None)
    }

    /// Replays accepted history while requiring the current signer for the unanchored tail.
    /// Error cases:
    /// - trusted history is inconsistent, or ordinary replay/recovery validation fails
    pub(crate) fn read_log_with_authorities(
        dir: &Path,
        domain: Option<crate::log::CheckpointDomain>,
        signer: Option<AccountId>,
        parent: Option<crate::log::CheckpointParent>,
        authorities: crate::checkpoints::SignerHistory,
        mut observe: impl FnMut(&LogEntry),
        marks: Option<crate::history::marks::MarkSink>,
    ) -> anyhow::Result<ReplayedLog> {
        let mut replay = parent
            .map_or_else(|| Replay::with_trust(domain, signer), |parent| Replay::with_recovery(domain, signer, parent));
        replay.set_authorities(authorities)?;
        if let Some(sink) = marks {
            replay.sample_marks_with(sink);
        }
        let log = SegmentedLog::open(dir, |entry| {
            observe(&entry);
            replay.apply(&entry)
        })?;
        Ok(ReplayedLog {
            log,
            replayed: replay.finish(),
            snapshot_seq: None,
            legacy_authorizations: legacy::Authorizations::new(),
        })
    }

    /// Recovers the log `read_log` already authenticated.
    /// Error cases:
    /// - the log records a client auth or an operator other than the config's, or a genesis it did
    ///   not apply in full (see `GenesisProgress::ensure_closed`), one no config loaded today can
    ///   give, or one other than the config's (see `GenesisProgress::ensure_matches`)
    /// - the log cannot be opened for appending, or written
    pub fn recover_from(
        log: ReplayedLog,
        cfg: SequencerConfig<'_>,
        key: Arc<EnclaveKey>,
        stream: broadcast::Sender<Arc<StreamMsg>>,
        withdrawals: mpsc::UnboundedSender<SignedWithdrawal>,
    ) -> anyhow::Result<Self> {
        log.validate_recovery(&cfg, AccountId(key.address()))?;
        let ReplayedLog { log, replayed, snapshot_seq, legacy_authorizations } = log;
        let Replayed {
            origin,
            engine,
            commands,
            checkpoints,
            unsigned,
            deferred,
            accounting,
            last_checkpoint_ts,
            last_signed_checkpoint,
            recovery_work,
            recovery_anchor_work,
            recovery_base,
            checkpoint_signers,
            genesis,
            activations,
            command_prefix,
            legacy_deposits,
            legacy_approvals,
            unreleased,
            payout_order,
            authorized,
            access_failed,
            last_marks,
            ..
        } = replayed;
        let replayed_last_marks = last_marks;
        let mut legacy_authorizations = legacy_authorizations;
        if ProtocolVersion::LATEST.supports_deposit_finality() {
            for (key, seq) in
                legacy_approvals.authorize(&engine, &legacy_deposits, &legacy::FinalityProof::default())?
            {
                legacy_authorizations.entry(key).or_insert(seq);
            }
        }
        let checkpoint_unsigned = cfg.checkpoints.is_some()
            && (!unsigned.is_empty() || !deferred.is_empty() || engine.withdrawal_approvals().next().is_some());
        info!(commands, checkpoints, snapshot_seq, "recovered from log");
        if let Some(ts) = last_checkpoint_ts {
            cfg.metrics.last_checkpoint_seconds.set(ts as f64 / 1000.0);
        }
        cfg.metrics.access_pending_deposits.set(engine.pending_deposits().len() as i64);
        let access_alerts = AccessAlerts::recovered(access_failed);
        let benchmark_schedule = watch::channel(BenchmarkSchedule::from_engine(&engine)).0;
        // The last checkpoint the log actually holds, for `Sequencer::checkpoint`'s
        // one-checkpoint-per-seq dedup: the missing trailing one is written on demand.
        let dedup_checkpoint_seq = last_signed_checkpoint.as_ref().map(|checkpoint| checkpoint.seq);
        let close_spread_ceilings = cfg.close_spread_ceilings.clone();
        let mut sequencer = Self {
            legacy_deposits,
            legacy_approvals,
            legacy_authorizations,
            next_seq: engine.last_seq().map_or(0, |s| s + 1),
            last_ts: engine.now(),
            last_checkpoint: engine.last_seq(),
            dedup_checkpoint_seq,
            engine,
            log: log.into_writer(cfg.segment_commands.max(1), cfg.manifest)?,
            key,
            command_link: origin.link,
            checkpoint_every: cfg.checkpoint_every.max(1),
            chain_id: cfg.chain_id,
            vault: cfg.vault,
            stream,
            withdrawals,
            outbox: Vec::new(),
            time: TimeGate::new(cfg.max_drift, cfg.clock.instant()),
            clock: cfg.clock,
            required: Vec::new(),
            client_auth: cfg.client_auth,
            access_policy: cfg.access_policy,
            close_spread_ceilings,
            deficit: DeficitAlert::default(),
            benchmark_alerts: BenchmarkAlerts::default(),
            access_alerts,
            operator: cfg.operator,
            metrics: cfg.metrics,
            gate: cfg.checkpoints.map(|sink| {
                let mut gate = Gate::new(sink, cfg.checkpoint_parent);
                gate.registry = cfg.checkpoint_registry;
                gate.approvals = cfg.approval_registry;
                gate.signing = cfg.checkpoint_signing;
                gate
            }),
            accounting,
            benchmark_sync: false,
            benchmark_sync_enabled: false,
            benchmark_time_refused: false,
            benchmark_schedule,
            benchmark_schedule_dirty: false,
            benchmark_schedule_pending: false,
            sealing: cfg.history_sealing,
            progress: Progress { genesis, activations, checkpoints },
            command_prefix,
            snapshot_base: recovery_base,
            snapshot_work: recovery_work.clone(),
            snapshot_anchor_work: recovery_anchor_work.clone(),
            snapshot_signers: checkpoint_signers,
            uncaptured: None,
            marks: Default::default(),
        };
        sequencer.marks.take_recovered(replayed_last_marks);
        {
            let _span = info_span!("command", id = sequencer.engine.last_seq()).entered();
            sequencer.deficit.note(sequencer.engine.house_nav());
            // A gated node pauses withdrawals until its first reconciliation proves the books.
            sequencer.metrics.withdrawals_paused.set(i64::from(sequencer.gate.is_some()));
        }
        for (account, amount, nonce) in unsigned {
            let _span = info_span!("command", id = sequencer.engine.last_seq()).entered();
            warn!(%account, nonce, "the log ends before this approved withdrawal was signed");
            if let Some(gate) = &mut sequencer.gate {
                gate.pending.push((sequencer.engine.last_seq().expect("approval has a seq"), account, amount, nonce));
            } else {
                sequencer.sign_withdrawal(account, amount, nonce)?;
            }
        }
        if sequencer.gate.is_none() {
            // A gated node re-signs durable approvals through the checkpoint worker; without a
            // gate, custody is the mock and takes the typed payout directly. The crash this
            // recovery serves can sit between the approval's checkpoint sync and its signature
            // sync, so that checkpoint may already be durable: write one only when the log lacks
            // it at this seq.
            let mut approval_checkpointed =
                last_signed_checkpoint.as_ref().is_some_and(|cp| Some(cp.seq) == sequencer.engine.last_seq());
            for approval in unreleased {
                let _span = info_span!("command", id = sequencer.engine.last_seq()).entered();
                warn!(account = %approval.account, nonce = approval.nonce, "the log ends before this approved withdrawal was signed");
                if !approval_checkpointed {
                    sequencer.checkpoint()?;
                    approval_checkpointed = true;
                }
                sequencer.sign_mock_authorization(approval)?;
            }
        }
        if let Some(gate) = &mut sequencer.gate {
            gate.pending.extend(deferred);
            anyhow::ensure!(
                payout_order.len() == sequencer.engine.withdrawal_approvals().count(),
                "immutable approval lacks its validated event order"
            );
            gate.backlog =
                payout_order.into_iter().map(|(key, index)| (key, index, authorized.get(&key).copied())).collect();
        }
        sequencer.load_backlog()?;
        if let Some(gate) = &mut sequencer.gate {
            // Resume every durable signature before extending the chain. In particular an
            // unmined child remains a live commitment even while latest equals finalized.
            for work in &recovery_work {
                gate.parent = crate::log::CheckpointParent {
                    has_checkpoint: true,
                    seq: work.checkpoint.seq,
                    root: work.checkpoint.root.parse()?,
                };
            }
            if let Some(registry) = &gate.registry {
                let mut registry =
                    registry.lock().map_err(|_| anyhow::anyhow!("checkpoint work registry is poisoned"))?;
                registry.extend(
                    recovery_anchor_work.iter().chain(&recovery_work).map(|work| (work.checkpoint.seq, work.clone())),
                );
            }
            gate.recovery = recovery_anchor_work.into_iter().chain(recovery_work).collect();
        }
        if checkpoint_unsigned {
            if sequencer
                .gate
                .as_ref()
                .is_some_and(|gate| gate.parent.has_checkpoint && Some(gate.parent.seq) == sequencer.engine.last_seq())
            {
                let cp = last_signed_checkpoint.context("finalized recovery parent lacks its signed checkpoint")?;
                if sequencer.gate.as_ref().is_some_and(|gate| gate.recovery.is_empty()) {
                    sequencer.queue_checkpoint(cp)?;
                }
            } else {
                sequencer.checkpoint()?;
            }
        }
        sequencer.prove_loaded()?;
        sequencer.flush()?;
        Ok(sequencer)
    }
}
