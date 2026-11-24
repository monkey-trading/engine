//! What each part of a carried genesis checks, and what it takes.

use std::collections::BTreeSet;

use alloy_primitives::B256;
use serde::Deserialize;

use crate::auth::ClientAuth;
use crate::awards::Award;
use crate::carry::{
    AbsenceProof, CarriedAward, CarriedBalance, CarryProgress, CarrySource, CarryStage, MAX_CARRY_ENTRIES, StateCarried,
};
use crate::emissions::{Emissions, MAX_PERIOD_BUDGET, TokenUnits};
use crate::engine::Engine;
use crate::event::Event;
use crate::merkle::{
    award_key, entitlement_hash, entitlement_key, globals_key, json_hash, keccak, staked_hash, staked_key, verify,
    verify_absent,
};
use crate::protocol::ProtocolVersion;
use crate::staking::{Staking, UsdcUnits};
use crate::types::AccountId;

/// The parts of a source's remaining-state leaf a carried genesis reads; it ignores the rest.
#[derive(Deserialize)]
pub(super) struct SourceView {
    protocol_version: ProtocolVersion,
    last_seq: Option<u64>,
    #[serde(default)]
    client_auth: Option<ClientAuth>,
    #[serde(default)]
    emissions: Option<Emissions>,
    #[serde(default)]
    staking: Option<Staking>,
    #[serde(default)]
    operator_election: ElectionView,
}

/// The part of the source's registry mirror a carried genesis reads.
#[derive(Default, Deserialize)]
struct ElectionView {
    #[serde(default)]
    rotation_enabled: bool,
}

impl SourceView {
    /// Parses a remaining-state leaf's JSON.
    /// Error cases:
    /// - JSON that does not hold the fields `SourceView` reads in their types
    pub(super) fn parse(globals: &str) -> Result<Self, String> {
        serde_json::from_str(globals)
            .map_err(|error| format!("the source's remaining-state leaf does not parse: {error}"))
    }

    /// The source's emissions and staking, once the leaf passes what `Begin` requires of `source`.
    /// Error cases:
    /// - client auth other than EIP-712 on `source`'s chain and vault, or a last seq other than `source`'s
    /// - a protocol before `V90`, whose root holds no total staked
    /// - emissions not initialised, or a period budget outside 1 to `MAX_PERIOD_BUDGET`
    /// - staking not initialised, or with no total staked
    /// - a staking mirror with no completed frontier, or one below a block it mirrored an event from
    /// - rotation's switch on in the registry mirror
    pub(super) fn check(self, source: &CarrySource) -> Result<(Emissions, Staking), String> {
        let auth = ClientAuth::Eip712 { chain_id: source.chain_id, vault: source.vault };
        if self.client_auth.as_ref() != Some(&auth) {
            return Err(format!(
                "the source does not record client auth on chain {} and vault {}",
                source.chain_id, source.vault
            ));
        }
        if self.last_seq != Some(source.seq) {
            return Err(format!("the source root is at seq {:?}, not {}", self.last_seq, source.seq));
        }
        if self.protocol_version < ProtocolVersion::V90 {
            return Err(format!(
                "the source is at {:?}, before V90, so its root holds no total staked",
                self.protocol_version
            ));
        }
        let emissions = self.emissions.ok_or("the source has not initialised emissions")?;
        let budget = emissions.schedule.period_budget.0;
        if budget == 0 || budget > MAX_PERIOD_BUDGET {
            return Err(format!("the source's period budget {budget} is outside 1 to {MAX_PERIOD_BUDGET}"));
        }
        let staking = self.staking.ok_or("the source has not initialised staking")?;
        if staking.staked_total.is_none() {
            return Err("the source's staking holds no total staked".into());
        }
        let complete = staking
            .completed_through
            .is_some_and(|completed| staking.mirrored_through.is_none_or(|mirrored| completed.block >= mirrored));
        if !complete {
            return Err("the source's staking mirror has not completed a scan through every block it mirrored".into());
        }
        if self.operator_election.rotation_enabled {
            return Err("rotation is on in the source's registry mirror".into());
        }
        Ok((emissions, staking))
    }
}

impl Engine {
    /// `Begin`: checks the source's remaining-state leaf against its root and starts the carry.
    /// Error cases:
    /// - a carry already begun or applied
    /// - anything `check_carry_destination` refuses
    /// - a leaf that does not prove against the source root, or that `SourceView::check` refuses
    pub(super) fn carry_begin(&mut self, source: &CarrySource, globals: &str, siblings: &[B256]) -> Result<(), String> {
        if self.carry.is_some() || self.carried_from.is_some() {
            return Err("this log's genesis has already carried state".into());
        }
        self.check_carry_destination()?;
        if !verify(source.root, globals_key(), keccak(&[globals.as_bytes()]), siblings) {
            return Err("the source's remaining-state leaf does not prove against its root".into());
        }
        let (emissions, staking) = SourceView::parse(globals)?.check(source)?;
        self.carry = Some(CarryProgress {
            source: *source,
            seq: self.last_seq.expect("a command is being applied"),
            emissions,
            mirrored_through: staking.mirrored_through,
            completed_through: staking.completed_through.expect("checked complete"),
            staked_total: staking.staked_total.expect("checked present"),
            stage: CarryStage::Begun,
            last_account: None,
            entitled: TokenUnits(0),
            staked: TokenUnits(0),
            next_award: 0,
        });
        Ok(())
    }

    /// Refuses a carry into this log unless staking is initialised under `V90` with nothing mirrored, and nothing
    /// is credited, staked, entitled, distributed or awarded: the log has run nothing but genesis.
    /// Error cases:
    /// - staking not initialised, initialised before `V90`, or with a mirrored event, frontier or distribution
    /// - emissions, an entitlement, a stake, a distribution, an award, an account, a credited or recorded deposit, or
    ///   house cash
    fn check_carry_destination(&self) -> Result<(), String> {
        let fresh = self.staking.as_ref().is_some_and(|staking| {
            staking.staked_total == Some(TokenUnits(0))
                && staking.distributed.is_zero()
                && staking.mirrored_through.is_none()
                && staking.completed_through.is_none()
        });
        if !fresh {
            return Err("a carried genesis needs staking initialised under V90, with nothing mirrored".into());
        }
        let untouched = self.emissions.is_none()
            && self.entitlements.is_empty()
            && self.staked.is_empty()
            && self.staking_refs.is_empty()
            && self.distributions.is_empty()
            && self.awards.is_empty()
            && self.accounts.is_empty()
            && self.credited_refs.is_empty()
            && self.deposits.is_empty()
            && self.pending_deposits.is_empty()
            && self.held_deposits.is_empty()
            && self.house_cash.is_zero();
        if !untouched {
            return Err(
                "a carried genesis applies only before anything is credited, staked, entitled or awarded".into()
            );
        }
        Ok(())
    }

    /// The carry in progress, for a part of `stage`.
    /// Error cases:
    /// - no carry begun
    /// - a part that is not the command after the carry's last
    /// - a part of a stage the carry has passed
    fn carry_at(&self, stage: CarryStage) -> Result<&CarryProgress, String> {
        let progress = self.carry.as_ref().ok_or("no carried genesis has begun")?;
        if self.last_seq != Some(progress.seq + 1) {
            return Err(format!(
                "a carried genesis's parts are consecutive commands; the last was seq {}",
                progress.seq
            ));
        }
        if progress.stage > stage {
            return Err(format!("a {stage:?} part comes after the carry reached {:?}", progress.stage));
        }
        Ok(progress)
    }

    /// An entitlements part: each entry the source root's entitlement leaf for its account.
    /// Error cases:
    /// - any error case of `carry_balances`
    pub(super) fn carry_entitlements(&mut self, entries: &[CarriedBalance]) -> Result<(), String> {
        self.carry_balances(CarryStage::Entitlements, entries)
    }

    /// A stakes part: each entry the source root's staked-balance leaf for its account, and positive.
    /// Error cases:
    /// - a zero stake, which no staked-balance leaf holds
    /// - any error case of `carry_balances`
    pub(super) fn carry_stakes(&mut self, entries: &[CarriedBalance]) -> Result<(), String> {
        if let Some(entry) = entries.iter().find(|entry| entry.amount.0 == 0) {
            return Err(format!("a carried stake is positive, and {}'s is zero", entry.account));
        }
        self.carry_balances(CarryStage::Stakes, entries)
    }

    /// Carries `entries` as entitlements or stakes, by `stage`, adding each to this log's balances.
    /// Error cases:
    /// - any error case of `carry_at`
    /// - no entries, or more than `MAX_CARRY_ENTRIES`
    /// - an account not after the last this stage carried
    /// - an entry whose leaf does not prove against the source root
    /// - a running sum past the source's total entitled or staked
    fn carry_balances(&mut self, stage: CarryStage, entries: &[CarriedBalance]) -> Result<(), String> {
        let progress = self.carry_at(stage)?;
        if entries.is_empty() || entries.len() > MAX_CARRY_ENTRIES {
            return Err(format!("a carried part holds 1 to {MAX_CARRY_ENTRIES} entries, not {}", entries.len()));
        }
        let mut last = if progress.stage == stage { progress.last_account } else { None };
        let (what, key, carried, limit): (&str, fn(AccountId) -> B256, TokenUnits, TokenUnits) = match stage {
            CarryStage::Entitlements => {
                ("entitlement", entitlement_key, progress.entitled, progress.emissions.entitled)
            }
            _ => ("stake", staked_key, progress.staked, progress.staked_total),
        };
        let mut total = carried.0;
        for entry in entries {
            if last.is_some_and(|last| entry.account <= last) {
                return Err(format!(
                    "carried {what}s are in strictly increasing account order, and {} is not",
                    entry.account
                ));
            }
            last = Some(entry.account);
            // Entitlement and staked-balance leaves hash their amount alike.
            let hash = if stage == CarryStage::Entitlements {
                entitlement_hash(entry.amount)
            } else {
                staked_hash(entry.amount)
            };
            if !verify(progress.source.root, key(entry.account), hash, &entry.siblings) {
                return Err(format!("the source root does not hold {} as {}'s {what}", entry.amount, entry.account));
            }
            total = total
                .checked_add(entry.amount.0)
                .filter(|total| *total <= limit.0)
                .ok_or_else(|| format!("the carried {what}s pass the source's total of {limit}"))?;
        }
        for entry in entries {
            match stage {
                CarryStage::Entitlements => {
                    self.entitlements.insert(entry.account, entry.amount);
                }
                _ => {
                    self.staked.insert(entry.account, entry.amount);
                }
            }
        }
        let next = self.carry.as_mut().expect("checked above");
        match stage {
            CarryStage::Entitlements => next.entitled = TokenUnits(total),
            _ => {
                next.staked = TokenUnits(total);
                let staking = self.staking.as_mut().expect("Begin checked staking");
                // `Begin` found it at zero, and the stakes' sum is checked below the source's total.
                staking.staked_total = Some(TokenUnits(total));
            }
        }
        (next.stage, next.last_account, next.seq) = (stage, last, next.seq + 1);
        Ok(())
    }

    /// An awards part: each award the source root's leaf for its id, from the next id in order, and with `next`,
    /// the proof the source holds no award at the id after them. Each carried award owes its whole amount.
    /// Error cases:
    /// - any error case of `carry_at`
    /// - more than `MAX_CARRY_ENTRIES` awards, or none without `next`
    /// - an id other than the next in order
    /// - an award, or the absence at the next id, that does not prove against the source root
    pub(super) fn carry_awards(&mut self, awards: &[CarriedAward], next: Option<&AbsenceProof>) -> Result<(), String> {
        let progress = self.carry_at(CarryStage::Awards)?;
        if awards.len() > MAX_CARRY_ENTRIES || (awards.is_empty() && next.is_none()) {
            return Err(format!(
                "an awards part holds up to {MAX_CARRY_ENTRIES} awards, or none with the next id's proof"
            ));
        }
        let root = progress.source.root;
        let mut id = progress.next_award;
        for carried in awards {
            if carried.id != id {
                return Err(format!("carried awards run from id 0 in order: {} where {id} is next", carried.id));
            }
            if !verify(root, award_key(id), json_hash(&carried.award), &carried.siblings) {
                return Err(format!("the source root does not hold the carried award {id}"));
            }
            id += 1;
        }
        if let Some(absence) = next
            && !verify_absent(root, award_key(id), absence.other, &absence.siblings)
        {
            return Err(format!("the proof does not show the source root holds no award at id {id}"));
        }
        for carried in awards {
            let unpaid = Award { paid: UsdcUnits(0), ..carried.award.clone() };
            self.awards.insert(carried.id, unpaid);
        }
        let progress = self.carry.as_mut().expect("checked above");
        progress.stage = if next.is_some() { CarryStage::AwardsClosed } else { CarryStage::Awards };
        (progress.next_award, progress.last_account, progress.seq) = (id, None, progress.seq + 1);
        Ok(())
    }

    /// `End`: checks every total, then takes the source's emissions and staking frontier and records the source.
    /// Error cases:
    /// - any error case of `carry_at`
    /// - awards not closed by the proof of the next id
    /// - entitlements or stakes that do not sum to the source's totals
    pub(super) fn carry_end(&mut self, events: &mut Vec<Event>) -> Result<(), String> {
        let progress = self.carry_at(CarryStage::AwardsClosed)?;
        if progress.stage != CarryStage::AwardsClosed {
            return Err("the carried awards end with the proof that the source holds none at the next id".into());
        }
        if progress.entitled != progress.emissions.entitled {
            return Err(format!(
                "the carried entitlements sum to {}, not the source's total entitled of {}",
                progress.entitled, progress.emissions.entitled
            ));
        }
        if progress.staked != progress.staked_total {
            return Err(format!(
                "the carried stakes sum to {}, not the source's total staked of {}",
                progress.staked, progress.staked_total
            ));
        }
        let progress = self.carry.take().expect("checked above");
        let staking = self.staking.as_mut().expect("Begin checked staking");
        staking.mirrored_through = progress.mirrored_through;
        staking.completed_through = Some(progress.completed_through);
        let (entitled, staked) = (progress.emissions.entitled, progress.staked);
        self.emissions = Some(progress.emissions);
        self.carried_from = Some(progress.source);
        let accounts: BTreeSet<&AccountId> = self.entitlements.keys().chain(self.staked.keys()).collect();
        let carried = StateCarried {
            source: progress.source,
            accounts: accounts.len() as u64,
            entitled,
            staked,
            awards: self.awards.len() as u64,
        };
        events.push(Event::StateCarried(carried));
        Ok(())
    }
}
