//! The parts of a carried genesis, built from a source engine at the root of its last command (`engine carry`).

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::B256;

use crate::auth::ClientAuth;
use crate::carry::apply::SourceView;
use crate::carry::{
    AbsenceProof, CarriedAward, CarriedBalance, CarryPart, CarrySource, MAX_CARRY_ENTRIES, StateCarried,
};
use crate::command::AdminCommand;
use crate::emissions::TokenUnits;
use crate::engine::Engine;
use crate::merkle::tree::{StateTree, Version};
use crate::merkle::{award_key, entitlement_key, globals_key, staked_key};
use crate::types::AccountId;

impl Engine {
    /// The parts of a genesis carrying this engine's state at its last root, which must be the root of its last
    /// command, in the canonical batches, and what they carry. Each part passes the checks a carried genesis makes
    /// of it, and every carried value is the one the root proves.
    /// Error cases:
    /// - no root of the last command, or the tree detached
    /// - client auth other than EIP-712
    /// - a source `Begin` refuses (`SourceView::check`): before `V90`, emissions or staking not initialised, a
    ///   staking mirror whose completed frontier misses a block it mirrored, or rotation's switch on
    /// - results in the open emission period, or an award a passed vote waits to apply, which a carry would lose
    pub fn carry_parts(&self) -> Result<(Vec<CarryPart>, StateCarried), String> {
        let (tree, version) = self.committed().ok_or("the engine holds no state root of its own")?;
        if self.last_seq != Some(version.seq()) {
            return Err(format!(
                "the engine's last root is at seq {}, not its last command {:?}",
                version.seq(),
                self.last_seq
            ));
        }
        let Some(ClientAuth::Eip712 { chain_id, vault }) = self.client_auth else {
            return Err("the source records no EIP-712 client auth, so it names no vault".into());
        };
        let source = CarrySource { chain_id, vault, seq: version.seq(), root: version.root() };
        let globals = self.globals_json();
        let (emissions, staking) = SourceView::parse(&globals)?.check(&source)?;
        if !self.period_results.is_empty() {
            return Err(format!(
                "{} accounts have results in the open emission period {}, which a carry would lose",
                self.period_results.len(),
                emissions.period
            ));
        }
        let pending =
            self.governance.pending.iter().find(|pending| matches!(pending.command, AdminCommand::ApproveAward(_)));
        if let Some(pending) = pending {
            return Err(format!("award proposal {} waits to apply, and a carry would lose it", pending.proposal));
        }
        let siblings = |key: B256| proven(tree, version, key);
        let mut parts = vec![CarryPart::Begin { source, globals, siblings: siblings(globals_key())? }];
        let balances = |map: &BTreeMap<AccountId, TokenUnits>, key: fn(AccountId) -> B256| {
            map.iter()
                .map(|(&account, &amount)| Ok(CarriedBalance { account, amount, siblings: siblings(key(account))? }))
                .collect::<Result<Vec<_>, String>>()
        };
        let entitlements = balances(&self.entitlements, entitlement_key)?;
        parts.extend(
            entitlements.chunks(MAX_CARRY_ENTRIES).map(|chunk| CarryPart::Entitlements { entries: chunk.to_vec() }),
        );
        let stakes = balances(&self.staked, staked_key)?;
        parts.extend(stakes.chunks(MAX_CARRY_ENTRIES).map(|chunk| CarryPart::Stakes { entries: chunk.to_vec() }));
        let awards = self
            .awards
            .iter()
            .map(|(&id, award)| Ok(CarriedAward { id, award: award.clone(), siblings: siblings(award_key(id))? }))
            .collect::<Result<Vec<_>, String>>()?;
        let next_id = self.awards.len() as u64;
        let (absent, other) = tree
            .absence(version, &award_key(next_id))
            .ok_or_else(|| format!("the root holds an award at id {next_id}, past the {next_id} awards"))?;
        let mut chunks: Vec<Vec<CarriedAward>> = awards.chunks(MAX_CARRY_ENTRIES).map(<[_]>::to_vec).collect();
        let last = chunks.pop().unwrap_or_default();
        parts.extend(chunks.into_iter().map(|awards| CarryPart::Awards { awards, next: None }));
        parts.push(CarryPart::Awards { awards: last, next: Some(AbsenceProof { other, siblings: absent }) });
        parts.push(CarryPart::End);
        let accounts: BTreeSet<&AccountId> = self.entitlements.keys().chain(self.staked.keys()).collect();
        let carried = StateCarried {
            source,
            accounts: accounts.len() as u64,
            entitled: emissions.entitled,
            staked: staking.staked_total.expect("SourceView::check found it"),
            awards: next_id,
        };
        Ok((parts, carried))
    }
}

/// The siblings of the leaf at `key` in `version`.
/// Error cases:
/// - the version holds no leaf at `key`
fn proven(tree: &StateTree, version: &Version, key: B256) -> Result<Vec<B256>, String> {
    tree.siblings(version, &key).ok_or_else(|| format!("the root holds no leaf at {key}"))
}
