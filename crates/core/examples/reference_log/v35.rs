//! The session's `V35` part: the operator's rate changes need a seven-day notice and stay within
//! ±50% of the launch rates, price rules cannot leave a market with open positions unpriced, and the
//! operator key rotates after a seven-day notice.

use engine_core::command::AdminCommand;
use engine_core::event::Event;
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice};
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::MarketId;

use crate::common::{Client, Harness};

/// The seed of the key the operator rotates to.
const NEXT_OPERATOR_SEED: u8 = 0x55;

/// `V35` activates over the parameters in force, whose profit fee and close-out level become the
/// launch rates. A rate change without a notice, a notice past +50% or less than seven days ahead,
/// and a change before a noticed time are refused, and the noticed change applies from its time. A relisting of a market
/// with open positions under a source count its venues cannot meet is refused until the market is
/// reduce-only. A rotation takes effect seven days after it is logged: the old key is refused from
/// then, and the new one applies.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V35 });
    let launch = h.engine.params().cloned().expect("parameters are set");
    let rates = |profit_fee_bps, closeout_level_bps| {
        AdminCommand::SetParams(ProtocolParams { profit_fee_bps, closeout_level_bps, ..launch.clone() })
    };
    let refused = |events: Vec<Event>| {
        assert!(matches!(events.last(), Some(Event::AdminRejected { .. })), "{:?}", events.last());
    };
    let notice = |closeout_level_bps, effective_at| {
        AdminCommand::AnnounceParams(ParamsNotice {
            profit_fee_bps: None,
            closeout_level_bps: Some(closeout_level_bps),
            effective_at,
        })
    };
    let fee = launch.profit_fee_bps;
    let level = launch.closeout_level_bps * 6 / 5;
    // A launch fee of zero admits no other fee, and a level change needs its notice.
    refused(h.operate(rates(fee + 1, launch.closeout_level_bps)));
    refused(h.operate(rates(fee, level)));
    refused(h.operate(notice(launch.closeout_level_bps * 2, h.ts + NOTICE_MS)));
    refused(h.operate(notice(level, h.ts + NOTICE_MS - 1)));
    let effective_at = h.ts + NOTICE_MS;
    h.operator_admin(notice(level, effective_at));
    h.advance(NOTICE_MS - 1);
    refused(h.operate(rates(fee, level)));
    h.advance_to(effective_at);
    h.operator_admin(rates(fee, level));
    assert_eq!(h.engine.params().map(|p| p.closeout_level_bps), Some(level));
    refused(h.operate(rates(fee, level + 1)));

    // A market some account holds a position in, priced by three venues.
    let held = |h: &Harness| -> MarketId {
        h.engine
            .markets()
            .values()
            .find(|market| {
                market.spec.schedule == Schedule::AlwaysOpen
                    && h.engine.accounts().values().any(|acct| acct.positions.contains_key(&market.spec.id))
            })
            .map(|market| market.spec.id)
            .expect("an always-open market with a position")
    };
    let id = held(h);
    let mid = h.engine.markets()[&id].mark.expect("a held market was marked").to_string();
    h.quotes(id, &[&mid, &mid, &mid]);
    let mut spec = h.engine.markets()[&id].spec.clone();
    spec.price_rules.min_sources = 4;
    refused(h.operate(AdminCommand::UpsertMarket(spec.clone())));
    h.operator_admin(AdminCommand::SetReduceOnly { market: Some(id), reduce_only: true });
    h.operator_admin(AdminCommand::UpsertMarket(spec.clone()));
    spec.price_rules.min_sources = 2;
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    h.operator_admin(AdminCommand::SetReduceOnly { market: Some(id), reduce_only: false });

    // The old key is refused from seven days after the rotation is logged, and the new one applies.
    let next = Client::new(NEXT_OPERATOR_SEED).account;
    let logged = h.ts;
    h.operator_admin(AdminCommand::RotateOperator { address: next });
    h.advance(NOTICE_MS - 1);
    h.operator_admin(AdminCommand::SetReduceOnly { market: Some(id), reduce_only: true });
    h.advance_to(logged + NOTICE_MS);
    refused(h.operate(AdminCommand::SetReduceOnly { market: Some(id), reduce_only: false }));
    let events = h.operate_as(NEXT_OPERATOR_SEED, AdminCommand::SetReduceOnly { market: Some(id), reduce_only: false });
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(next));
}
