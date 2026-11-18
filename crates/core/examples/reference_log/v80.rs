//! The session's `V80` part: a rollover's financing as one aggregate event. A vote activates the
//! version. A client goes long BTC and short ETH; the next rollover logs one `financing_applied` in place
//! of a `financing` event per position, and derives the rows: the long pays, the short is credited, the
//! silver long on the unrated "Sofr" accrues nothing, the rows sum to the event's net, and the root over
//! them is the logged one, against which the client's proof verifies. Two rollovers missed until one
//! `Tick` log one event each, the rows of each under its own root. BTC then falls until the client's
//! equity sits just above its close-out level, and the next rollover's charge closes it out.

use alloy_primitives::B256;
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::financing::{self, FinancingRow, leaf_hash, root_of};
use engine_core::fixed::Rounding;
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, MarketId, Side};

use crate::common::close_spread::{attest, fund, trade};
use crate::common::events::only_fill;
use crate::common::{BTC, Client, ETH, Harness};
use crate::v56::pass;

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x80;
/// The first of this part's proposal ids.
const PROPOSAL: u64 = 220;
const DAY_MS: i64 = 24 * 3_600_000;

/// Activates `V80` by vote; then the rollovers described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V80 };
    let events = pass(h, PROPOSAL, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: PROPOSAL, command: activation })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V80);
    // An activation leaves the rollovers its notice spanned to the next command, which charges them.
    let events = h.cmd(Command::Tick);
    rollovers(h, events);

    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    fund(h, &client, "12000");
    let (btc, eth) = (mark(h, BTC), mark(h, ETH));
    h.price(BTC, &btc.to_string());
    h.price(ETH, &eth.to_string());
    only_fill(&trade(h, &client, BTC, Side::Buy, &worth(100_000, btc), &btc.to_string()));
    only_fill(&trade(h, &client, ETH, Side::Sell, &worth(3_000, eth), &eth.to_string()));

    // The next rollover: one event, the long charged, the short credited.
    let due = h.engine.next_rollover().expect("rollovers are scheduled");
    h.advance_to(due);
    let events = h.cmd(Command::Tick);
    let charged = rollovers(h, events);
    let [(rows, proof_root)] = &charged[..] else { panic!("one rollover: {charged:?}") };
    let own = |market: MarketId| {
        rows.iter().find(|row| row.account == client.account && row.market == market).map(|row| row.amount)
    };
    assert!(own(BTC).expect("the long is charged").is_positive());
    assert!(own(ETH).expect("the short is charged").is_negative(), "the benchmark is above ETH's markup");
    let proof = financing::proof(rows, client.account).expect("the client was charged");
    assert!(proof.verify(*proof_root), "the client's proof verifies against the logged root");

    // Two rollovers missed until one command: an event and a root each.
    h.advance_to(due + 2 * DAY_MS);
    let events = h.cmd(Command::Tick);
    assert_eq!(rollovers(h, events).len(), 2, "two rollovers caught up");

    // BTC falls until the client's equity is just above its close-out level; the next rollover's
    // charge takes it below, and the client is closed out after the rollover's event.
    let due = h.engine.next_rollover().expect("rollovers are scheduled");
    h.advance_to(due - 1);
    let edge = edge_of_closeout(h, client.account, BTC);
    h.price(BTC, &edge.to_string());
    h.price(ETH, &mark(h, ETH).to_string());
    let held = h.engine.accounts()[&client.account].positions.len();
    assert_eq!(held, 2, "the price leaves the client above its close-out level");
    h.advance_to(due);
    let events = h.cmd(Command::Tick);
    let applied = events.iter().position(|e| matches!(e, Event::FinancingApplied { .. })).expect("the rollover");
    let closed = events.iter().position(|e| matches!(e, Event::CloseOut { account, .. } if *account == client.account));
    assert!(closed.is_some_and(|closed| closed > applied), "the charge closes the client out: {events:?}");
    rollovers(h, events);
    let held = &h.engine.accounts()[&client.account].positions;
    assert!(!held.contains_key(&BTC), "the close-out takes the losing long first");
}

/// Checks each `FinancingApplied` among `events` against the rows the command derived, in turn: no
/// per-position event is logged; each event's rows number its positions, sum to its net, name its
/// accounts, charge no market whose benchmark the rollover found without a rate though accounts hold
/// it, and their root is its root. Returns each rollover's rows and root.
fn rollovers(h: &mut Harness, events: Vec<Event>) -> Vec<(Vec<FinancingRow>, B256)> {
    assert!(!events.iter().any(|e| matches!(e, Event::Financing { .. })), "no per-position event is logged");
    let derived = h.engine.take_derived();
    let unrated: Vec<MarketId> = events
        .iter()
        .flat_map(|e| match e {
            Event::BenchmarkRateMissing { markets, .. } => markets.clone(),
            _ => Vec::new(),
        })
        .collect();
    let mut rest = &derived[..];
    let mut out = Vec::new();
    for event in &events {
        let Event::FinancingApplied { positions, accounts, net, root, .. } = event else { continue };
        let (rows, after) = rest.split_at(usize::try_from(*positions).expect("a count"));
        rest = after;
        let charges = financing::charges_of(rows);
        assert_eq!(charges.len() as u64, *accounts);
        assert_eq!(rows.iter().map(|row| row.amount).sum::<Fixed>(), *net);
        assert_eq!(root_of(charges.iter().map(|(account, charges)| leaf_hash(*account, charges)).collect()), *root);
        assert!(!rows.iter().any(|row| unrated.contains(&row.market)), "an unrated market accrues nothing");
        out.push((rows.to_vec(), *root));
    }
    assert!(rest.is_empty(), "every derived row belongs to a logged rollover");
    if !out.is_empty() {
        let held = |market: &MarketId| h.engine.accounts().values().any(|a| a.positions.contains_key(market));
        assert!(unrated.iter().any(held), "a market on the unrated benchmark is held, and skipped: {unrated:?}");
    }
    out
}

/// The quantity, to a thousandth, worth about `usd` at `mark`.
fn worth(usd: i64, mark: Fixed) -> String {
    let thousandths = Fixed::from_int(usd).div(mark).raw() / (Fixed::ONE.raw() / 1_000);
    Fixed::from_int(i64::try_from(thousandths).expect("a quantity")).div_int(1_000).to_string()
}

fn mark(h: &Harness, market: MarketId) -> Fixed {
    h.engine.markets()[&market].mark.expect("the market has traded")
}

/// The lowest price of `market`, to the cent, at which `account` holds its close-out level, its other
/// markets at their marks: the close-out check's own test, an equity at or above the requirement at the
/// close-out level.
fn edge_of_closeout(h: &Harness, account: AccountId, market: MarketId) -> Fixed {
    let acct = &h.engine.accounts()[&account];
    let level_bps = h.engine.params().expect("parameters are set").closeout_level_bps;
    let price = |cents: i64| Fixed::from_int(cents).div_int(100);
    let holds = |cents: i64| {
        let (equity, margin) = h.engine.equity_and_margin(acct, Some((market, price(cents), None)));
        equity >= margin.mul_bps_rounded(level_bps, Rounding::Up)
    };
    let (mut low, mut high) = (1, i64::try_from(mark(h, market).raw() * 100 / Fixed::ONE.raw()).expect("cents"));
    assert!(holds(high) && !holds(low), "the client holds its level at the mark, and not at a cent");
    while low + 1 < high {
        let mid = (low + high) / 2;
        if holds(mid) { high = mid } else { low = mid }
    }
    price(high)
}
