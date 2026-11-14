//! The leaf of the engine's remaining state.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::Fixed;
use crate::access::AccessPolicy;
use crate::auth::{ClientAuth, Operator};
use crate::benchmark::BenchmarkHistory;
use crate::close_spread::PendingCeiling;
use crate::deposits::CustodyCheckpoint;
use crate::election::OperatorElection;
use crate::emissions::Emissions;
use crate::engine::Engine;
use crate::governance::Governance;
use crate::house_loss::HouseLoss;
use crate::merkle::state::is_zero;
use crate::operator_bounds::OperatorBounds;
use crate::params::ProtocolParams;
use crate::protocol::ProtocolVersion;
use crate::staking::Staking;
use crate::turns::DailyTurns;
use crate::types::{MarketId, Millis, OrderId};

/// The engine fields no leaf of their own holds.
#[derive(Serialize)]
pub(super) struct Globals<'a> {
    params: &'a Option<ProtocolParams>,
    house_cash: Fixed,
    benchmarks: &'a BTreeMap<String, i64>,
    global_stress_floor: usize,
    next_order_id: OrderId,
    next_rollover: Option<Millis>,
    last_seq: Option<u64>,
    now: Millis,
    protocol_version: ProtocolVersion,
    client_auth: &'a Option<ClientAuth>,
    operator: &'a Option<Operator>,
    global_reduce_only: bool,
    markout_base: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    benchmark_history: &'a BenchmarkHistory,
    #[serde(skip_serializing_if = "Option::is_none")]
    emissions: &'a Option<Emissions>,
    #[serde(skip_serializing_if = "Fixed::is_zero")]
    unfinalised_house_deposits: Fixed,
    #[serde(skip_serializing_if = "Option::is_none")]
    custody_checkpoint: Option<CustodyCheckpoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    withdrawal_cash: Option<Fixed>,
    #[serde(skip_serializing_if = "is_zero")]
    withdrawal_next_ticket: u64,
    /// From `V25`; omitted until staking is initialised, so earlier roots stand.
    #[serde(skip_serializing_if = "Option::is_none")]
    staking: &'a Option<Staking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    access_policy: &'a Option<AccessPolicy>,
    #[serde(skip_serializing_if = "OperatorBounds::is_default")]
    operator_bounds: &'a OperatorBounds,
    /// From `V37`; omitted until the registry's first mirrored event, so earlier roots stand.
    #[serde(skip_serializing_if = "OperatorElection::is_default")]
    operator_election: &'a OperatorElection,
    /// From `V51`; omitted until the schedule's first mirrored event, so earlier roots stand.
    #[serde(skip_serializing_if = "DailyTurns::is_default")]
    daily_turns: &'a DailyTurns,
    /// From `V56`; omitted until the vote contract is recorded, so earlier roots stand.
    #[serde(skip_serializing_if = "Governance::is_default")]
    governance: &'a Governance,
    /// From `V40`; omitted until the registry is installed, so earlier roots stand.
    #[serde(skip_serializing_if = "Option::is_none")]
    close_spread_ceilings: &'a Option<BTreeMap<MarketId, i64>>,
    /// From `V40`; omitted while no adoption waits out its notice, so earlier roots stand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_ceiling: &'a Option<PendingCeiling>,
    #[serde(skip_serializing_if = "HouseLoss::is_default")]
    house_loss: &'a HouseLoss,
}

impl Engine {
    /// The fields the remaining-state leaf holds.
    pub(super) fn globals(&self) -> Globals<'_> {
        // Every field is named, so a field added to `Engine` fails to compile until a leaf holds it.
        let Engine {
            params,
            markets: _,
            accounts: _,
            house_cash,
            benchmarks,
            credited_refs: _,
            deposits: _,
            deposit_transactions: _,
            legacy_deposits: _,
            custody_checkpoint,
            unfinalised_house_deposits,
            global_stress_floor,
            next_order_id,
            next_rollover,
            markouts: _,
            last_seq,
            now,
            protocol_version,
            client_auth,
            request_nonces: _,
            operator,
            global_reduce_only,
            markout_base,
            benchmark_history,
            emissions,
            period_results: _,
            entitlements: _,
            emission_ledgers: _,
            withdrawal_cash,
            withdrawal_queue: _,
            withdrawal_next_ticket,
            withdrawal_approvals: _,
            corporate_actions: _,
            corporate_refs: _,
            staking,
            staking_refs: _,
            staked: _,
            distributions: _,
            access_policy,
            operator_bounds,
            operator_election,
            daily_turns,
            governance,
            close_spread_ceilings,
            pending_ceiling,
            session_keys: _,
            toxicity_scores: _,
            markout_spreads: _,
            house_loss,
            eligibility: _,
            held_deposits: _,
            pending_deposits: _,
            changed: _,
            tree: _,
            command_ms: _,
            hot_indexes: _,
        } = self;
        Globals {
            params,
            house_cash: *house_cash,
            benchmarks,
            global_stress_floor: *global_stress_floor,
            next_order_id: *next_order_id,
            next_rollover: *next_rollover,
            last_seq: *last_seq,
            now: *now,
            protocol_version: *protocol_version,
            client_auth,
            operator,
            global_reduce_only: *global_reduce_only,
            markout_base: *markout_base,
            benchmark_history,
            emissions,
            unfinalised_house_deposits: *unfinalised_house_deposits,
            custody_checkpoint: *custody_checkpoint,
            withdrawal_cash: *withdrawal_cash,
            withdrawal_next_ticket: *withdrawal_next_ticket,
            staking,
            access_policy,
            operator_bounds,
            operator_election,
            daily_turns,
            governance,
            close_spread_ceilings,
            pending_ceiling,
            house_loss,
        }
    }
}
