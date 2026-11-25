//! The carried replay state kept up to date entry by entry beside an engine that applies the same commands,
//! as a replay folds it: the stats worker keeps one over its copy of the engine, seeded from the replay a
//! start ends with, so the node's daily snapshots carry what a replay of the log holds at their checkpoints
//! with no work on the sequencer's thread. It checks nothing: the log it folds is the node's own, and every
//! replay of it compares a snapshot's state with its own (`Replay::carried`).

use std::collections::{BTreeMap, VecDeque};

use anyhow::{Context, ensure};
use engine_core::command::Sequenced;
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};

use crate::log::{Checkpoint, PrefixRing};
use crate::sequencer::SignedWithdrawal;
use crate::sequencer::replay::live::Link;
use crate::sequencer::replay::{Carried, Replay};

/// The replay state after the entries folded so far.
#[derive(Clone, Debug)]
pub(crate) struct Fold {
    /// All of the state but `links` and `authorized`, which are kept here as a replay keeps them.
    state: Carried,
    links: VecDeque<Link>,
    authorized: BTreeMap<(AccountId, u64), SignedWithdrawal>,
    /// Approvals the last command made that the engine does not keep, awaiting their withdrawal entries.
    approved: VecDeque<(AccountId, Fixed, u64)>,
    /// The seq of the checkpoint folded last, while no entry has followed it.
    at_checkpoint: Option<u64>,
}

impl Fold {
    /// The fold `replay` holds after the entry it applied last.
    /// Error cases:
    /// - the replay is of one range of a log or from a recovery cache that carries no replay state, or of a log
    ///   whose commands carry no events
    pub(in crate::sequencer) fn of(replay: &Replay) -> anyhow::Result<Self> {
        ensure!(
            replay.range.is_none() && (!replay.partial || replay.cached.is_some()),
            "a replay of one range of a log, or from a recovery cache that carries no replay state, holds no whole fold"
        );
        let (payout_order, signed, authorized) = match &replay.cached {
            Some(cached) => cached.whole(replay),
            None => (replay.payout_order.clone(), replay.signed.clone(), replay.authorized.clone()),
        };
        ensure!(replay.logs_events || replay.commands == 0, "a fold is of a log whose commands carry events");
        let at_checkpoint = replay
            .last_signed_checkpoint
            .as_ref()
            .filter(|_| replay.last_offset.is_some() && replay.last_offset == replay.last_checkpoint_offset)
            .map(|checkpoint| checkpoint.seq);
        let state = Carried {
            commands: replay.commands,
            command_prefix: B256::from(replay.command_prefix),
            links: Vec::new(),
            checkpoints: replay.checkpoints,
            accounting: replay.accounting,
            genesis: replay.genesis.clone(),
            activations: replay.activations.clone(),
            params_seq: replay.params_seq,
            checkpoint_signers: replay.signers_seen.clone(),
            checkpoint_roots: replay.payout_checkpoints.clone(),
            takings: replay.takings.applied_throughout(),
            payout_order,
            signed,
            authorized: Vec::new(),
            deferred: replay.deferred.clone(),
            requires_checkpoint_gate: replay.requires_checkpoint_gate,
        };
        let (links, approved) = (replay.recent.clone(), replay.approved.clone());
        Ok(Self { state, links, authorized, approved, at_checkpoint })
    }

    /// The state a daily snapshot of the checkpoint at `seq`, folded last, carries.
    /// Error cases:
    /// - the entry folded last is not a checkpoint at `seq`
    pub(crate) fn carried(&self, seq: u64) -> anyhow::Result<Carried> {
        ensure!(
            self.at_checkpoint == Some(seq) && self.approved.is_empty(),
            "the fold is not at the checkpoint at seq {seq}, but after {:?}",
            self.links.back().map(|link| link.seq)
        );
        let mut carried = self.state.clone();
        carried.links = self.links.iter().copied().collect();
        carried.authorized = self.authorized.values().cloned().collect();
        Ok(carried)
    }

    /// The SHA-256 of everything the fold holds, by which a test compares two folds of one log.
    #[cfg(test)]
    pub(crate) fn digest(&self) -> [u8; 32] {
        let mut state = self.state.clone();
        state.links = self.links.iter().copied().collect();
        state.authorized = self.authorized.values().cloned().collect();
        let encoded = serde_json::to_vec(&(state, &self.approved, self.at_checkpoint)).expect("a fold encodes");
        <sha2::Sha256 as sha2::Digest>::digest(encoded).into()
    }

    /// Folds a command `engine` applied, `sequenced`, which gave `events` and moved the engine from protocol
    /// `before`, with its cumulative command prefix `link` and its seal's signer, as a replay folds it.
    /// Error cases:
    /// - the checkpoint accounting refuses the events
    pub(crate) fn command(
        &mut self,
        engine: &Engine,
        sequenced: &Sequenced,
        events: &[Event],
        before: ProtocolVersion,
        (link, sealer): (B256, Option<AccountId>),
    ) -> anyhow::Result<()> {
        let (seq, ts, state) = (sequenced.seq, sequenced.ts, &mut self.state);
        state.genesis.command(sequenced, events);
        if crate::sequencer::house::applied_set_params(&sequenced.cmd, events) {
            state.params_seq = Some(seq);
        }
        if engine.protocol_version() != before {
            state.activations.push((engine.protocol_version(), seq));
        }
        state.accounting.apply(&sequenced.cmd, events).with_context(|| format!("folding seq {seq}"))?;
        state.takings.stamped(seq, ts);
        state.takings.command(seq, events);
        let kept = |account: AccountId, nonce: u64| engine.withdrawal_approval(account, nonce).is_some();
        for (index, event) in events.iter().enumerate() {
            if let Event::WithdrawalApproved { account, nonce, .. } = event
                && kept(*account, *nonce)
            {
                state.payout_order.push(((*account, *nonce), index));
            }
        }
        self.approved = events
            .iter()
            .filter_map(|event| match event {
                Event::WithdrawalApproved { account, amount, nonce } if !kept(*account, *nonce) => {
                    Some((*account, *amount, *nonce))
                }
                _ => None,
            })
            .collect();
        state.commands += 1;
        state.command_prefix = link;
        self.links.push_back(Link { seq, link, ts, sealer });
        if self.links.len() > PrefixRing::CAPACITY {
            self.links.pop_front();
        }
        self.at_checkpoint = None;
        Ok(())
    }

    /// Folds a withdrawal entry, as a replay folds the one it accepts.
    /// Error cases:
    /// - an untyped entry signs neither a deferred approval nor the next one its command made
    pub(crate) fn withdrawal(&mut self, withdrawal: &SignedWithdrawal) -> anyhow::Result<()> {
        let key = (withdrawal.account, withdrawal.nonce);
        if withdrawal.authorization.is_some() {
            self.authorized.insert(key, withdrawal.clone());
        } else if let Some(seq) = withdrawal.approved_seq {
            let deferred = (seq, withdrawal.account, withdrawal.amount, withdrawal.nonce);
            let at = self.state.deferred.iter().position(|value| *value == deferred);
            self.state.deferred.remove(at.context("the fold holds no such deferred approval")?);
        } else {
            ensure!(
                self.approved.front() == Some(&(withdrawal.account, withdrawal.amount, withdrawal.nonce)),
                "the fold holds no such approval awaiting its signature"
            );
            self.approved.pop_front();
        }
        self.state.signed.insert(key);
        self.at_checkpoint = None;
        Ok(())
    }

    /// Folds a checkpoint entry, as a replay folds the one it accepts.
    /// Error cases:
    /// - the checkpoint's root or signer does not parse
    pub(crate) fn checkpoint(&mut self, checkpoint: &Checkpoint) -> anyhow::Result<()> {
        let state = &mut self.state;
        if checkpoint.deferred_withdrawals {
            let approved = self.approved.drain(..);
            state.deferred.extend(approved.map(|(account, amount, nonce)| (checkpoint.seq, account, amount, nonce)));
        }
        state.requires_checkpoint_gate |= checkpoint.parent.is_some();
        state.checkpoint_roots.insert(checkpoint.seq, checkpoint.root.parse().context("the checkpoint's root")?);
        state.checkpoint_signers.insert(checkpoint.signer.parse().context("the checkpoint's signer")?);
        state.checkpoints += 1;
        state.genesis.checkpoint();
        self.at_checkpoint = Some(checkpoint.seq);
        Ok(())
    }
}
