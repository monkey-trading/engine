//! The session's `V55` part: a realised gain pays the profit fee its hold gives, the hold running from
//! the position's open time, and the base fee and the hold curve move only up, after a notice.

use engine_core::access::AccessDeposit;
use engine_core::command::{AdminCommand, Command};
use engine_core::eip712;
use engine_core::event::{Event, Fill};
use engine_core::holding_fee::{CurvePoint, DEFAULT_PROFIT_FEE_CURVE};
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice};
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::close_spread::{attest, evidence, fund, reduce, trade};
use crate::common::{BTC, Client, Harness, f};
use crate::v51::MEMBER_SEEDS;

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x64;

fn sign(h: &mut Harness, command: AdminCommand) -> Vec<Event> {
    h.operate_as(MEMBER_SEEDS[0], command)
}

/// A notice of `curve` as the profit fee curve, from `effective_at`.
fn curve_notice(curve: Vec<CurvePoint>, effective_at: i64) -> AdminCommand {
    AdminCommand::AnnounceParams(ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress: None,
        profit_fee_curve: Some(curve),
        effective_at,
        score_tiers: None,
    })
}

/// `V55` activates over a held BTC long, which starts at the curve's last hold, so its close pays the
/// base, zero since an earlier part of the session. A position opened after it pays the whole gain on a
/// close 5 s later, and an add moves the open time. A curve below launch's is refused, as is a raised one
/// with no notice; a noticed raise applies at its time, and a close then pays the raised curve.
pub(crate) fn session(h: &mut Harness) {
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "1000000");
    let house = AccessDeposit {
        account: None,
        payer: Some(client.account),
        amount: f("2000000"),
        chain_ref: "0xhouse:55".into(),
    };
    let ev = evidence(h, eip712::deposit_screening_hash(&house), None, Some(client.account));
    h.cmd(Command::AccessDeposit { deposit: house, evidence: ev });
    h.price(BTC, "80000");
    only_fill(&trade(h, &client, BTC, Side::Buy, "0.2", "80000"));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V55 });
    h.price(BTC, "81000");
    let held = only_fill(&reduce(h, &client, BTC, Side::Sell, "0.2", "81000"));
    assert_eq!((held.hold_ms, held.profit_fee_bps), (Some(900_000), Some(0)), "{held:?}");

    h.price(BTC, "80000");
    only_fill(&trade(h, &client, BTC, Side::Buy, "0.1", "80000"));
    h.advance(5_000);
    h.price(BTC, "81000");
    let whole = only_fill(&reduce(h, &client, BTC, Side::Sell, "0.1", "81000"));
    assert_eq!(
        (whole.hold_ms, whole.profit_fee_bps, whole.profit_fee),
        (Some(5_000), Some(10_000), whole.realised_pnl)
    );

    only_fill(&trade(h, &client, BTC, Side::Buy, "0.2", "81000"));
    h.advance(60_000);
    h.price(BTC, "81000");
    only_fill(&trade(h, &client, BTC, Side::Buy, "0.1", "81000"));
    h.advance(5_000);
    h.price(BTC, "82000");
    // Two tenths held 65 s and one 5 s: the open time sits 45 s back, where the curve charges 70.37%.
    let close = only_fill(&reduce(h, &client, BTC, Side::Sell, "0.3", "82000"));
    assert_eq!((close.hold_ms, close.profit_fee_bps), (Some(45_000), Some(7_037)), "{close:?}");

    let due = h.ts + NOTICE_MS;
    let mut lowered = DEFAULT_PROFIT_FEE_CURVE.to_vec();
    lowered[0].fee_bps -= 1;
    refused(&sign(h, curve_notice(lowered, due)));
    let raised: Vec<CurvePoint> =
        DEFAULT_PROFIT_FEE_CURVE.iter().map(|p| CurvePoint { fee_bps: (p.fee_bps + 500).min(10_000), ..*p }).collect();
    let params =
        ProtocolParams { profit_fee_curve: raised.clone(), ..h.engine.params().expect("parameters are set").clone() };
    refused(&sign(h, AdminCommand::SetParams(params.clone())));
    applied(&sign(h, curve_notice(raised, due)));
    refused(&sign(h, AdminCommand::SetParams(params.clone())));
    h.advance_to(due);
    applied(&sign(h, AdminCommand::SetParams(params)));
    h.price(BTC, "80000");
    only_fill(&trade(h, &client, BTC, Side::Buy, "0.1", "80000"));
    h.advance(600_000);
    h.price(BTC, "81000");
    let close = only_fill(&reduce(h, &client, BTC, Side::Sell, "0.1", "81000"));
    assert_eq!(close.profit_fee_bps, Some(2_328), "{close:?}");
}

fn only_fill(events: &[Event]) -> Fill {
    crate::common::events::only_fill(events).clone()
}

fn refused(events: &[Event]) {
    assert!(matches!(events.last(), Some(Event::AdminRejected { .. })), "{events:?}");
}

fn applied(events: &[Event]) {
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
}
