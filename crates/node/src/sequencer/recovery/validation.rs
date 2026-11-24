//! Pure recovery validation of authenticated durable checkpoint work.

use super::*;

impl ReplayedLog {
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
        // The config's key is the successor's, on this deployment, when a pending rotation (a promoted or elected
        // standby's), an election still to take effect (from `V88`, waiting or the last behind them), a daily turn
        // assigned to it (before or after it applies) or a promotion names it.
        let turns = engine.daily_turns();
        let turn_member = cfg.promoted_operator == Some(cfg.operator.address)
            || turns.assigned.values().chain(&turns.in_effect).any(|turn| turn.admin_key == cfg.operator.address);
        let named_successor = (turn_member
            || engine.operator_bounds().rotation.is_some_and(|rotation| rotation.address == cfg.operator.address)
            || engine.awaited_election_keys().any(|key| key == cfg.operator.address))
            && engine.operator().is_some_and(same_domain);
        // So does the key the finalised registry installed while rotation is off, before the log's
        // scan has passed the installation: the fallback for a failed operator boots under its config.
        let installed = cfg.registry_operator.and_then(|seated| seated.unscanned(engine)) == Some(cfg.operator.address);
        if let Some(recorded) = engine.operator().filter(|recorded| {
            **recorded != cfg.operator && !named_successor && !((rotated || installed) && same_domain(recorded))
        }) {
            bail!("the log records operator {recorded:?}, but the config gives {:?}", cfg.operator);
        }
        if let Some(recorded) = engine.access_policy()
            && cfg.access_policy.as_ref() != Some(recorded)
        {
            bail!("the configured access policy differs from the public log");
        }
        crate::sequencer::required::refuse_other_contracts(engine, cfg)?;
        // A registry the `V40` upgrade carried came from the config of the restart that upgraded
        // the log: the recorded genesis cannot be restated, so the carried entries are not part
        // of what the genesis comparison attests. For each ceiling the config names, the log may
        // enforce no more: the node may not attest ceilings stricter than it applies. Entries the
        // log later gained by signed adoption need no config entry — they only added a market or
        // lowered one.
        if let Some(registry) = engine.close_spread_ceilings()
            && !genesis.has_ceiling_install()
        {
            for ceiling in &cfg.close_spread_ceilings {
                if let Some(&adopted) = registry.get(&ceiling.market) {
                    anyhow::ensure!(
                        adopted <= ceiling.max_close_spread_bps,
                        "the log's close-spread ceiling for market {} is {adopted}bps, above this config's {}bps; \
                         start the node under the config the upgrade carried",
                        ceiling.market,
                        ceiling.max_close_spread_bps,
                    );
                }
            }
        }
        genesis.ensure_closed()?;
        // So is one naming another genesis: the node would attest rules its engine does not run.
        genesis.ensure_matches(&cfg.genesis, &cfg.close_spread_ceilings, &cfg.client_auth, cfg.operator)?;
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
