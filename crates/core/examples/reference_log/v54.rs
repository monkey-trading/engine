//! The session's `V54` part: the toxicity score as a stick. An activation whose launch score tiers
//! would lift a close charge over its ceiling is refused; one over parameters naming a lower tier
//! clears the earlier scores, takes those tiers and gives BTC-USD a crypto listing's markout floor,
//! which a relisting keeps; a trader whose buys the index rises from a horizon later pays no add-on
//! until twenty fills have scored and then its tier's add-on; a round trip at the index scores
//! nothing; a markout due in a venue gap scores at the first index after it; and the tiers move
//! after a seven-day notice.

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice};
use engine_core::protocol::ProtocolVersion;
use engine_core::toxicity::{GRID_MS, HORIZON_MS, MIN_FILLS, ScoreTier};
use engine_core::types::Side;

use crate::common::close_spread::{attest, fund, trade};
use crate::common::events::only_fill;
use crate::common::{BTC, Client, Harness, f};
use crate::v51::MEMBER_SEEDS;

/// The seed of the key this part trades through.
const TRADER_SEED: u8 = 0x63;

/// Applies an admin command signed by the turn's member key `V51`'s part left in effect.
fn operate(h: &mut Harness, command: AdminCommand) -> Vec<Event> {
    h.operate_as(MEMBER_SEEDS[0], command)
}

/// The trader's market buy of 0.01 BTC at `mid`, and the spread it paid.
fn buy(h: &mut Harness, trader: &Client, mid: Fixed) -> i64 {
    only_fill(&trade(h, trader, BTC, Side::Buy, "0.01", &mid.to_string())).spread_bps
}

/// The parameters name no tier since `V29`, so the launch score tiers' 5 bps would lift AAPL's close
/// charge over its 15 bps ceiling, which cannot be raised: that activation is refused. Parameters
/// naming one tier adding 1 bp fit, and `V54` activates over them and the earlier parts' scores,
/// and gives BTC-USD, listed with no floor, a crypto listing's 1 bp: a relisting keeps it, and
/// another floor is refused. Off the sampling grid, so the floor is the markout spread, a trader buys
/// and the index rises 2 bps a horizon later, 22 times: the first 20 buys pay no add-on and the rest
/// the tier's. A round trip at the index then scores nothing, and a buy whose
/// markout falls due while the quotes are stale scores at the next price. A notice of tiers within
/// their launch bounds is refused as a change before its seven days and applied after them.
pub(crate) fn session(h: &mut Harness) {
    let activation = Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V54 });
    let refused = h.cmd(activation.clone());
    let cannot_raise = |reason: &str| reason.contains("a ceiling cannot be raised");
    assert!(matches!(refused.last(), Some(Event::AdminRejected { reason }) if cannot_raise(reason)), "{refused:?}");
    let tiers = vec![ScoreTier { min_score: f("4"), extra_spread_bps: 1 }];
    let mut params = h.engine.params().unwrap().clone();
    params.toxicity.score_tiers = tiers.clone();
    assert_eq!(operate(h, AdminCommand::SetParams(params)).last(), Some(&Event::AdminApplied));
    assert_eq!(h.cmd(activation).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.params().unwrap().toxicity.score_tiers, tiers);
    let floored = h.engine.markets()[&BTC].spec.clone();
    assert_eq!(floored.markout_floor_bps, Some(f("1")));
    assert_eq!(operate(h, AdminCommand::UpsertMarket(floored.clone())).last(), Some(&Event::AdminApplied));
    let moved = MarketSpec { markout_floor_bps: Some(f("2")), ..floored };
    assert!(matches!(operate(h, AdminCommand::UpsertMarket(moved)).last(), Some(Event::AdminRejected { .. })));

    let trader = Client { expires_at: h.ts + 60_000, ..Client::new(TRADER_SEED) };
    attest(h, &trader);
    fund(h, &trader, "100000");
    h.advance_to(h.ts.div_euclid(GRID_MS) * GRID_MS + GRID_MS + GRID_MS / 2);
    let mut mid = f("80000");
    h.price(BTC, &mid.to_string());
    let mut paid = Vec::new();
    for _ in 0..MIN_FILLS + 2 {
        paid.push(buy(h, &trader, mid));
        mid += mid.mul_bps(2);
        h.advance(HORIZON_MS);
        h.price(BTC, &mid.to_string());
    }
    // From `V52` every fill pays its band's spread: the first buy, with no add-on, pays only that.
    let band = paid[0];
    assert_eq!(paid.iter().filter(|bps| **bps == band).count(), MIN_FILLS as usize, "{paid:?}");
    assert_eq!(paid.last(), Some(&(band + 1)), "{paid:?}");
    let toxic = h.engine.toxicity_standing(trader.account).unwrap();

    for side in [Side::Buy, Side::Sell] {
        only_fill(&trade(h, &trader, BTC, side, "0.5", &mid.to_string()));
    }
    h.advance(HORIZON_MS);
    h.price(BTC, &mid.to_string());
    assert_eq!(h.engine.toxicity_standing(trader.account).unwrap(), toxic);

    buy(h, &trader, mid);
    h.advance(HORIZON_MS);
    h.cmd(Command::Tick);
    h.advance(2_000);
    h.price(BTC, &mid.to_string());
    assert_eq!(h.engine.toxicity_standing(trader.account).unwrap().fills, toxic.fills + Fixed::ONE);

    let raised: Vec<ScoreTier> =
        tiers.into_iter().map(|tier| ScoreTier { min_score: tier.min_score + Fixed::ONE, ..tier }).collect();
    let mut params = h.engine.params().unwrap().clone();
    params.toxicity.score_tiers = raised.clone();
    let refused = operate(h, AdminCommand::SetParams(params.clone()));
    assert!(matches!(refused.last(), Some(Event::AdminRejected { .. })), "{refused:?}");
    let effective_at = h.ts + NOTICE_MS;
    let notice = ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress: None,
        score_tiers: Some(raised),
        effective_at,
    };
    assert_eq!(operate(h, AdminCommand::AnnounceParams(notice)).last(), Some(&Event::AdminApplied));
    h.advance_to(effective_at);
    assert_eq!(operate(h, AdminCommand::SetParams(params)).last(), Some(&Event::AdminApplied));
}
