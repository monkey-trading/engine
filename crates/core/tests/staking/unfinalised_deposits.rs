//! From `V75`, a client's unfinalised deposit is the client's own cash at an epoch close. A deposit is
//! credited at the sequencer's confirmation and unlocks at Ethereum finality, so one made in the minutes
//! before a close is still unfinalised at it. The close reserves only what each account's cash, and its
//! marked equity, would fall below zero by were its deposits to reverse, so a deposit parked over a close
//! moves nothing to a later close, its stake snapshot or its operator. `V74` still reserves the whole
//! deposit from house cash and house NAV.

use alloy_primitives::B256;
use engine_core::Fixed;
use engine_core::access::receipt_access_deposit;
use engine_core::command::Command;
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis, Side};

use crate::common::close_spread::{attest, evidence, fund, trade};
use crate::common::events::fills;
use crate::common::{BTC, Client, Harness, f, operator};
use crate::support::{DAY_MS, EXCESS, USDC, close_at, closed, elect, end_of, frontier, harness, paid, stake};

const MINUTE_MS: Millis = 60 * 1000;
const KEY_SEED: u8 = 0x66;
/// The client that deposits. It holds no stake and no office.
const PARKER_SEED: u8 = 71;
/// A client funded with final cash, the other side of the parker's trades.
const COUNTERPARTY_SEED: u8 = 72;
const CANDIDATE: AccountId = AccountId([0xc1; 20]);
const HONEST_STAKER: AccountId = AccountId([0xa1; 20]);
const LATE_STAKER: AccountId = AccountId([0xa2; 20]);
const V74: ProtocolVersion = ProtocolVersion::V74;
const V75: ProtocolVersion = ProtocolVersion::V75;

/// `client`'s vault deposit of `amount` USDC, credited on the sequencer's confirmation and not yet final
/// on Ethereum, as the node sequences a deposit made a minute ago. `client` has declared.
fn park(h: &mut Harness, client: &Client, amount: &str) -> DepositReceipt {
    let receipt = DepositReceipt {
        block: 9_000 + h.log.len() as u64,
        block_hash: B256::repeat_byte(0x91),
        tx_hash: B256::repeat_byte(0x92),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 0,
        beneficiary: DepositBeneficiary::Trader { account: client.account },
        amount: f(amount),
    };
    let deposit = receipt_access_deposit(&receipt, Some(client.account));
    let ev = evidence(h, engine_core::eip712::deposit_screening_hash(&deposit), deposit.account, deposit.payer);
    let house_before = h.engine.house_cash();
    let events = h.cmd(Command::AccessCustodyDeposit { receipt: receipt.clone(), payer: deposit.payer, evidence: ev });
    assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { .. })), "{events:?}");
    // The deposit is the client's own cash, and its principal is locked; the house holds none of it.
    assert_eq!(h.engine.accounts()[&client.account].unfinalised_deposits, f(amount));
    assert_eq!(h.engine.house_cash(), house_before);
    receipt
}

/// A declared client that parks `amount` USDC.
fn parker(h: &mut Harness, amount: &str) -> (Client, DepositReceipt) {
    let client = Client::new(PARKER_SEED);
    // Whatever was due by now (sessions, rollovers) runs first, so the next commands stand alone.
    h.cmd(Command::Tick);
    attest(h, &client);
    let receipt = park(h, &client, amount);
    (client, receipt)
}

/// Ethereum finality reaches the parked deposit.
fn finalise(h: &mut Harness, receipt: DepositReceipt) {
    let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).expect("recorded").updated_at_seq;
    let events = h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Finalise { receipt, observed_at_seq })));
    assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { .. })), "{events:?}");
}

/// Two epochs under `version`: the deployer's key in office for the whole of the first, the elected
/// candidate for the whole of the second, and the house's 1,000 USDC of excess there throughout the
/// first. With `parked`, a client that is neither staker nor operator deposits 1,000 USDC a minute before
/// the first epoch ends, final five minutes after it. Returns what the first operator, the second operator
/// and the staker hold after both closes.
fn two_operators(version: ProtocolVersion, parked: bool) -> (u128, u128, u128) {
    let mut h = harness(version);
    let start = end_of(&h, 7);
    let first_end = start + DAY_MS;
    elect(&mut h, CANDIDATE, KEY_SEED, first_end);
    // The close that opens the first epoch: nothing is staked, so it pays no one and starts the count.
    assert_eq!(closed(&close_at(&mut h, start)), Some((EXCESS, 0)));
    stake(&mut h, HONEST_STAKER, 1);
    // The mirror reaches the epoch's end ahead of the close, so the election hands over with it.
    h.advance_to(first_end - 2 * MINUTE_MS);
    frontier(&mut h, first_end);
    let receipt = parked.then(|| {
        h.advance_to(first_end - MINUTE_MS);
        parker(&mut h, "1000").1
    });
    close_at(&mut h, first_end);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
    if let Some(receipt) = receipt {
        h.advance_to(first_end + 5 * MINUTE_MS);
        finalise(&mut h, receipt);
    }
    close_at(&mut h, first_end + DAY_MS);
    (paid(&h, operator().address), paid(&h, CANDIDATE), paid(&h, HONEST_STAKER))
}

#[test]
fn a_deposit_unfinalised_at_a_close_leaves_the_operators_share_with_the_operator_that_served() {
    // No deposit: the operator that served the first epoch earns its fifth, 200 USDC, and the staker the
    // rest; the second epoch has nothing new to pay.
    for version in [V74, V75] {
        assert_eq!(two_operators(version, false), (200_000_000, 0, 800_000_000), "{version:?}");
    }
    assert_eq!(two_operators(V75, true), (200_000_000, 0, 800_000_000));
    // `V74` reserved the deposit, so the candidate in office one epoch later took the 200 USDC.
    assert_eq!(two_operators(V74, true), (0, 200_000_000, 800_000_000));
}

/// Two epochs under `version`. One holder is staked through both; a second stakes the same amount only
/// after the first closes. The house's 1,000 USDC of excess is there throughout the first epoch, and
/// the second adds nothing. With `parked`, a client deposits 1,000 USDC a minute before the first epoch
/// ends. Returns what the honest and the late staker hold after both closes.
fn two_stakers(version: ProtocolVersion, parked: bool) -> (u128, u128) {
    let mut h = harness(version);
    let start = end_of(&h, 7);
    assert_eq!(closed(&close_at(&mut h, start)), Some((EXCESS, 0)));
    stake(&mut h, HONEST_STAKER, 1);
    let first_end = start + DAY_MS;
    let receipt = parked.then(|| {
        h.advance_to(first_end - MINUTE_MS);
        parker(&mut h, "1000").1
    });
    close_at(&mut h, first_end);
    stake(&mut h, LATE_STAKER, 1);
    if let Some(receipt) = receipt {
        h.advance_to(first_end + 5 * MINUTE_MS);
        finalise(&mut h, receipt);
    }
    close_at(&mut h, first_end + DAY_MS);
    (paid(&h, HONEST_STAKER), paid(&h, LATE_STAKER))
}

#[test]
fn a_deposit_unfinalised_at_a_close_gives_a_later_staker_nothing_of_that_epoch() {
    for version in [V74, V75] {
        assert_eq!(two_stakers(version, false), (800_000_000, 0), "{version:?}");
    }
    assert_eq!(two_stakers(V75, true), (800_000_000, 0));
    // `V74` held the epoch's excess back to the close after the late staker joined.
    assert_eq!(two_stakers(V74, true), (400_000_000, 400_000_000));
}

#[test]
fn a_deposit_unfinalised_at_a_close_leaves_the_excess_whole() {
    for (version, after_deposit, paid) in [(V75, EXCESS, EXCESS), (V74, 0, 0)] {
        let mut h = harness(version);
        let start = end_of(&h, 7);
        assert_eq!(closed(&close_at(&mut h, start)), Some((EXCESS, 0)));
        stake(&mut h, HONEST_STAKER, 1);
        let end = start + DAY_MS;
        h.advance_to(end - MINUTE_MS);
        h.cmd(Command::Tick);
        assert_eq!(h.engine.distributable_excess(), Some(usdc(EXCESS)));
        parker(&mut h, "1000");
        assert_eq!(h.engine.distributable_excess(), Some(usdc(after_deposit)), "{version:?}");
        assert_eq!(closed(&close_at(&mut h, end)), Some((paid, paid)), "{version:?}");
    }
}

/// What the parked client does with its unfinalised 1,000 USDC before finality.
#[derive(Clone, Copy, Debug)]
enum Use {
    Nothing,
    /// Buys 0.01 BTC at 100,000 and sells it at 95,000, realising a loss the house holds in cash.
    RealisedLoss,
    /// Sells 0.01 BTC at 100,000 and buys it back at 95,000, realising a gain the house paid.
    RealisedGain,
    /// Buys 0.01 BTC at 100,000 against the counterparty's 0.02 short, and BTC falls to 95,000: its
    /// marked equity falls below the deposit while the clients' net unrealised gain holds house NAV
    /// below house cash.
    UnrealisedLoss,
    /// `RealisedLoss` while the counterparty holds a 0.02 long through the fall, whose unrealised loss
    /// holds house NAV above house cash, so the cash reserve binds alone.
    RealisedLossWithTheHouseAhead,
}

#[test]
fn a_client_trading_on_an_unfinalised_deposit_moves_the_excess_by_what_its_reversal_would_cost() {
    let uses =
        [Use::Nothing, Use::RealisedLoss, Use::RealisedGain, Use::UnrealisedLoss, Use::RealisedLossWithTheHouseAhead];
    for use_of in uses {
        let mut h = harness(V75);
        h.price(BTC, "100000");
        let counterparty = Client::new(COUNTERPARTY_SEED);
        attest(&mut h, &counterparty);
        fund(&mut h, &counterparty, "1000000");
        let (client, receipt) = parker(&mut h, "1000");
        let traded = |events: Vec<Event>| assert_eq!(fills(&events).len(), 1, "{use_of:?}: {events:?}");
        match use_of {
            Use::Nothing => {}
            Use::RealisedLoss | Use::RealisedGain | Use::RealisedLossWithTheHouseAhead => {
                if matches!(use_of, Use::RealisedLossWithTheHouseAhead) {
                    traded(trade(&mut h, &counterparty, BTC, Side::Buy, "0.02", "100000"));
                }
                let (open, close) =
                    if matches!(use_of, Use::RealisedGain) { (Side::Sell, Side::Buy) } else { (Side::Buy, Side::Sell) };
                traded(trade(&mut h, &client, BTC, open, "0.01", "100000"));
                h.price(BTC, "95000");
                traded(trade(&mut h, &client, BTC, close, "0.01", "95000"));
                if matches!(use_of, Use::RealisedLossWithTheHouseAhead) {
                    assert!(h.engine.house_nav() > h.engine.house_cash(), "the counterparty is behind");
                }
            }
            Use::UnrealisedLoss => {
                traded(trade(&mut h, &counterparty, BTC, Side::Sell, "0.02", "100000"));
                traded(trade(&mut h, &client, BTC, Side::Buy, "0.01", "100000"));
                h.price(BTC, "95000");
                assert!(h.engine.house_nav() < h.engine.house_cash(), "the clients are ahead, net");
            }
        }
        let account = &h.engine.accounts()[&client.account];
        let (cash, equity) = (account.balance, h.engine.equity_and_margin(account, None).0);
        // What the house would bear were the deposit to reverse: the client's own cash and equity take
        // it back first.
        let shortfall = match use_of {
            Use::Nothing | Use::RealisedGain => Fixed::ZERO,
            Use::RealisedLoss | Use::RealisedLossWithTheHouseAhead => f("1000") - cash,
            Use::UnrealisedLoss => f("1000") - equity,
        };
        if !matches!(use_of, Use::Nothing | Use::RealisedGain) {
            assert!(shortfall > f("50"), "{use_of:?}: {shortfall}");
        }
        let unfinalised = h.engine.distributable_excess().expect("staking");
        finalise(&mut h, receipt);
        let finalised = h.engine.distributable_excess().expect("staking");
        assert_eq!(finalised - unfinalised, shortfall, "{use_of:?}");
    }
}

/// `units` USDC base units as `Fixed`.
fn usdc(units: u128) -> Fixed {
    Fixed::from_raw(units as i128 * USDC)
}
