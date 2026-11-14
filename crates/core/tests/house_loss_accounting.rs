//! What `V59`'s house loss counts, through `Engine::apply`: capital a vault receipt credits the house
//! or reverses and a distribution out of it are no loss, and a cash dividend paid at its ex-date open
//! is no loss until its market takes a fresh mark, whatever its price, a split in between included.

mod common;
#[path = "corporate_actions/support.rs"]
mod support;

use alloy_primitives::B256;
use common::close_spread::{ceilings, evidence, install_expect};
use common::house_loss::{harness, loss};
use common::{AAPL, HOUSE_CAPITAL, Harness, acct, f};
use engine_core::Fixed;
use engine_core::access::receipt_access_deposit;
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712;
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Millis;
use support::{accepted, close, dividend, held, open, payment, schedule, tick};

const DAY_MS: Millis = 24 * 60 * 60 * 1000;

#[test]
fn a_house_receipt_credited_or_reversed_is_no_loss() {
    let (mut h, client) = harness(1_000, "100000");
    let receipt = DepositReceipt {
        block: 2,
        block_hash: B256::repeat_byte(1),
        tx_hash: B256::repeat_byte(2),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 1,
        beneficiary: DepositBeneficiary::House,
        amount: f("100"),
    };
    let (before, cash) = (loss(&h), h.engine.house_cash());
    // The vault's deposit for the house, screened as its payer's.
    let payer = Some(client.account);
    let screening = eip712::deposit_screening_hash(&receipt_access_deposit(&receipt, payer));
    let evidence = evidence(&h, screening, None, payer);
    custody(&mut h, Command::AccessCustodyDeposit { receipt: receipt.clone(), payer, evidence });
    assert_eq!((h.engine.house_cash() - cash, loss(&h)), (f("100"), before));
    let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).expect("credited").updated_at_seq;
    custody(&mut h, Command::CustodyDeposit(Box::new(DepositUpdate::Reverse { receipt, observed_at_seq })));
    assert_eq!((h.engine.house_cash(), loss(&h)), (cash, before));
}

#[test]
fn a_distribution_out_of_the_house_is_not_a_loss() {
    // A 1,000 USDC excess over the floor, closed under `V59` with one staker.
    let floor = Fixed::from_int(HOUSE_CAPITAL - 1_000);
    let mut h = Harness::staking(floor, DAY_MS, 3 * DAY_MS);
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 });
    let (block, chain_ref) = (h.log.len() as u64, "0xstake:1".to_string());
    h.cmd(Command::Stake { account: acct(1), amount: TokenUnits(1_000), block, chain_ref });
    let before = h.engine.house_loss().expect("V59").loss;
    let cash = h.engine.house_cash();
    let staking = h.engine.staking().expect("initialised");
    h.advance_to(staking.config.epoch_end(staking.config.epochs_elapsed(h.ts)));
    let events = h.cmd(Command::Tick);
    assert!(events.iter().any(|e| matches!(e, Event::DistributionAllocated { .. })), "{events:?}");
    assert_eq!(cash - h.engine.house_cash(), f("1000"));
    assert_eq!(h.engine.house_loss().expect("V59").loss, before);
}

#[test]
fn a_dividend_paid_before_its_market_reprices_is_no_loss() {
    let (mut h, before) = dividend_on_a_long(false);
    // At the ex-date open the house pays the long its 6 while AAPL's mark is still the close's 80,
    // which includes the 4 a unit: without it, AAPL leaves the house 2 ahead, the withholding it keeps.
    let events = tick(&mut h, open());
    assert_eq!(payment(&events, 1), f("6"));
    assert_eq!(loss(&h), before - f("2"));
    // AAPL's first quote, 4 lower, takes the dividend and leaves the loss where it was.
    h.price(AAPL, "76");
    assert_eq!(loss(&h), before - f("2"));
}

#[test]
fn a_fresh_mark_takes_the_dividend_at_any_price() {
    let (mut h, before) = dividend_on_a_long(false);
    tick(&mut h, open());
    assert_eq!(loss(&h), before - f("2"));
    // A first quote at the close's 80 again means the price did not fall: the 6 paid is a loss.
    h.price(AAPL, "80");
    assert_eq!(loss(&h), before + f("6"));
}

#[test]
fn a_split_at_the_dividends_open_leaves_its_value() {
    let (mut h, before) = dividend_on_a_long(true);
    // Four for one at the same open: the long holds 8 at a mark of 20, and the dividend is still the 8
    // its 2 units were paid on.
    tick(&mut h, open());
    assert_eq!(h.engine.markets()[&AAPL].mark, Some(f("20")));
    assert_eq!(loss(&h), before - f("2"));
    h.price(AAPL, "19");
    assert_eq!(loss(&h), before - f("2"));
}

// ── Helpers ──

/// A client long 2 AAPL at 80 under `V59`, a dividend of 4 a unit with 25% withheld at the next open,
/// and with `split` a four-for-one split at the same open, ticked to the close: the harness and the
/// house's loss there.
fn dividend_on_a_long(split: bool) -> (Harness, Fixed) {
    let mut h = held("2", "0");
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 });
    accepted(&schedule(&mut h, dividend("cash", AAPL, "4", "0.25")));
    if split {
        accepted(&schedule(&mut h, support::split("stock", 4, 1)));
    }
    tick(&mut h, close());
    let before = loss(&h);
    (h, before)
}

/// Sequences a custody command, which must not be refused.
fn custody(h: &mut Harness, command: Command) {
    let events = h.cmd(command);
    assert!(!events.iter().any(|event| matches!(event, Event::CustodyDepositRejected { .. })), "{events:?}");
}
