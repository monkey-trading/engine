//! Public FIFO reservations, exact payments and physical liquidity through `Engine::apply`.

mod common;
#[path = "withdrawals/setup.rs"]
mod setup;

use common::{BTC, ETH, acct, f};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;
use engine_core::withdrawal::{MAX_WITHDRAWAL_APPROVALS_PER_COMMAND, from_usdc, usdc_amount};
use setup::{approvals, capital, deposit, v22, withdraw};

#[test]
fn full_withdrawals_reserve_once_and_allocate_distinct_payout_nonces() {
    let mut h = v22();
    let a = acct(1);
    deposit(&mut h, a, "100");
    let requested_seq = h.log.len() as u64;
    assert_eq!(
        withdraw(&mut h, a, "60"),
        [
            Event::WithdrawalQueued { account: a, amount: f("60"), requested_seq },
            Event::WithdrawalApproved { account: a, amount: f("60"), nonce: 1 },
            Event::WithdrawalQueueUpdated { account: a, requested_seq, remaining: Fixed::ZERO },
        ]
    );
    assert_eq!(h.balance(a), f("40"));
    assert_eq!(h.engine.withdrawal_cash(), Some(f("50")));
    assert!(h.engine.withdrawal_queue().next().is_none());
    let first = *h.engine.withdrawal_approval(a, 1).unwrap();
    assert_eq!((first.approved_seq, first.amount_usdc()), (requested_seq, 60_000_000));
    assert_eq!(approvals(&withdraw(&mut h, a, "10")), [(a, f("10"), 2)]);
    assert_eq!(h.balance(a), f("30"));
    assert_eq!(h.engine.withdrawal_approval(a, 1), Some(&first));
    assert_eq!(setup::cash_and_reservations(&h.engine), f("40"));
}

#[test]
fn partial_head_finishes_before_a_follower_and_each_part_has_its_own_nonce() {
    let mut h = setup::two_waiting();
    let (a, b) = (acct(1), acct(2));
    let snapshot = || h.engine.withdrawal_queue().map(|q| (q.account, q.remaining)).collect::<Vec<_>>();
    assert_eq!(snapshot(), [(a, f("80")), (b, f("110"))]);
    assert_eq!((h.balance(a), h.balance(b)), (Fixed::ZERO, Fixed::ZERO));
    assert_eq!(h.engine.withdrawal_cash(), Some(Fixed::ZERO));
    assert!(h.engine.house_cash().is_negative());

    let paid = deposit(&mut h, acct(3), "40");
    assert_eq!(approvals(&paid), [(a, f("40"), 2)]);
    assert!(h.engine.withdrawal_approval(b, 1).is_none());
    let paid = capital(&mut h, "60");
    assert_eq!(approvals(&paid), [(a, f("40"), 3), (b, f("20"), 1)]);
    let queue = h.engine.withdrawal_queue().copied().collect::<Vec<_>>();
    assert_eq!(queue.len(), 1);
    assert_eq!((queue[0].account, queue[0].remaining), (b, f("90")));
    assert_eq!(approvals(&deposit(&mut h, acct(4), "90")), [(b, f("90"), 2)]);
    assert!(h.engine.withdrawal_queue().next().is_none());
    assert_eq!(h.engine.withdrawal_cash(), Some(Fixed::ZERO));
    assert_eq!(h.engine.withdrawal_approvals().map(|a| a.amount).sum::<Fixed>(), f("220"));
    assert_eq!(setup::cash_and_reservations(&h.engine), Fixed::ZERO);
}

#[test]
fn reserved_cash_cannot_margin_trades_or_absorb_trading_losses() {
    let mut h = v22();
    let a = acct(1);
    deposit(&mut h, a, "10");
    setup::win(&mut h, a);
    withdraw(&mut h, a, "100");
    assert_eq!(h.balance(a), f("10"));
    assert_eq!(h.engine.withdrawal_queue().next().unwrap().remaining, f("80"));

    h.price(ETH, "100");
    let rejected = h.market_order(a, ETH, Side::Buy, "2", "100");
    assert!(
        rejected.iter().any(|e| matches!(e,
            Event::OrderRejected { reason: Reject::InsufficientMargin { equity, required }, .. }
            if *equity == f("10") && *required == f("20")
        )),
        "{rejected:?}"
    );
    setup::assert_filled(&h.market_order(a, ETH, Side::Buy, "1", "100"));
    let loss = h.price(ETH, "0.1");
    assert!(loss.iter().any(|e| matches!(e, Event::BadDebt { account, .. } if *account == a)));
    assert!(approvals(&loss).is_empty());
    assert_eq!(h.balance(a), Fixed::ZERO);
    assert_eq!(h.engine.withdrawal_queue().next().unwrap().remaining, f("80"));
    assert_eq!(h.engine.withdrawal_cash(), Some(Fixed::ZERO));
    assert_eq!(setup::cash_and_reservations(&h.engine), Fixed::ZERO);
}

#[test]
fn each_command_is_bounded_and_a_new_request_cannot_pass_the_backlog() {
    let mut h = v22();
    let a = acct(1);
    deposit(&mut h, a, "10");
    setup::win(&mut h, a);
    withdraw(&mut h, a, "20");
    for _ in 0..20 {
        assert!(approvals(&withdraw(&mut h, a, "1")).is_empty());
    }
    let seqs = h.engine.withdrawal_queue().map(|q| q.requested_seq).collect::<Vec<_>>();
    let events = capital(&mut h, "21");
    assert_eq!(approvals(&events).len(), MAX_WITHDRAWAL_APPROVALS_PER_COMMAND);
    let completed = events
        .iter()
        .filter_map(|e| match e {
            Event::WithdrawalQueueUpdated { requested_seq, remaining, .. } if remaining.is_zero() => {
                Some(*requested_seq)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed, seqs[..MAX_WITHDRAWAL_APPROVALS_PER_COMMAND]);
    assert_eq!(h.engine.withdrawal_queue().count(), 4);

    let events = withdraw(&mut h, a, "1");
    assert_eq!(approvals(&events).len(), 5);
    let completed = events
        .iter()
        .filter_map(|e| match e {
            Event::WithdrawalQueueUpdated { requested_seq, .. } => Some(*requested_seq),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed[..4], seqs[16..]);
    assert_eq!(completed[4], h.engine.last_seq().unwrap());
    assert_eq!(h.engine.withdrawal_cash(), Some(Fixed::ZERO));
    assert!(h.engine.withdrawal_queue().next().is_none());
}

#[test]
fn a_realized_loss_or_tick_retries_the_head_without_manufacturing_physical_cash() {
    for trading in [true, false] {
        let mut h = v22();
        let a = acct(1);
        deposit(&mut h, a, "10");
        setup::win(&mut h, a);
        withdraw(&mut h, a, "20");
        for _ in 0..20 {
            withdraw(&mut h, a, "1");
        }
        h.price(ETH, "100");
        setup::assert_filled(&h.market_order(a, ETH, Side::Buy, "1", "100"));
        h.price(ETH, "99");
        let funded = capital(&mut h, "20");
        assert_eq!(approvals(&funded).len(), MAX_WITHDRAWAL_APPROVALS_PER_COMMAND);
        assert_eq!(h.engine.withdrawal_cash(), Some(f("4")));
        assert_eq!(h.engine.withdrawal_queue().count(), 4);

        let events = if trading {
            let events = h.market_order(a, ETH, Side::Sell, "1", "99");
            assert!(events.iter().any(|e| matches!(e, Event::Fill(fill) if fill.realised_pnl == f("-1"))));
            events
        } else {
            h.cmd(Command::Tick)
        };
        assert_eq!(approvals(&events), (18..=21).map(|nonce| (a, f("1"), nonce)).collect::<Vec<_>>());
        assert_eq!(h.engine.withdrawal_cash(), Some(Fixed::ZERO));
        assert!(h.engine.withdrawal_queue().next().is_none());
        assert_eq!(setup::cash_and_reservations(&h.engine), Fixed::ZERO);
    }
}

#[test]
fn whole_usdc_is_debited_and_dust_remains_in_the_trading_account() {
    let mut h = v22();
    let a = acct(1);
    deposit(&mut h, a, "1.000000999");
    assert_eq!(approvals(&withdraw(&mut h, a, "1.000000999")), [(a, f("1"), 1)]);
    assert_eq!(h.balance(a), f("0.000000999"));
    assert_eq!(h.engine.withdrawal_approval(a, 1).unwrap().amount_usdc(), 1_000_000);
    for amount in ["0", "-1", "0.000000999", "1000000000.000000001"] {
        let events = withdraw(&mut h, a, amount);
        assert_eq!(
            events,
            [Event::WithdrawalRejected { account: a, amount: f(amount), reason: Reject::InvalidAmount }]
        );
    }
    assert_eq!(h.balance(a), f("0.000000999"));
    assert_eq!(usdc_amount(f("0.000001")), Some(1));
    assert_eq!(usdc_amount(f("0.000000001")), None);
    assert_eq!(usdc_amount(f("-0.000001")), None);
    assert_eq!(usdc_amount(Fixed::ZERO), Some(0));
    assert_eq!(from_usdc(0), Some(Fixed::ZERO));
    assert_eq!(from_usdc(1), Some(f("0.000001")));
    assert_eq!(from_usdc(u128::MAX), None);
    let max_units = (i128::MAX as u128) / 1_000;
    assert_eq!(usdc_amount(from_usdc(max_units).unwrap()), Some(max_units));
    assert_eq!(from_usdc(max_units + 1), None);
    assert_eq!(usdc_amount(Fixed::MAX_INPUT), Some(1_000_000_000_000_000));
}

#[test]
fn a_stale_held_market_uses_its_last_mark_for_withdrawal_and_other_market_margin() {
    let mut h = v22();
    let a = acct(1);
    deposit(&mut h, a, "1000");
    h.price(ETH, "4000");
    setup::assert_filled(&h.market_order(a, ETH, Side::Buy, "1", "4000"));
    h.price(ETH, "3800");
    h.advance(1_001);
    h.price(BTC, "100000");
    assert_eq!(
        withdraw(&mut h, a, "420.000001"),
        [Event::WithdrawalRejected {
            account: a,
            amount: f("420.000001"),
            reason: Reject::InsufficientFunds { available: f("420") },
        }]
    );
    assert_eq!(approvals(&withdraw(&mut h, a, "120")), [(a, f("120"), 1)]);
    setup::assert_filled(&h.market_order(a, BTC, Side::Buy, "0.03", "100000"));
    assert_eq!(h.engine.equity_and_margin(&h.engine.accounts()[&a], None), (f("680"), f("680")));
}

#[test]
fn activation_reserves_legacy_approvals_and_later_legacy_gaps_remain_conservative() {
    let mut h = setup::predecessor();
    let a = acct(1);
    deposit(&mut h, a, "10");
    setup::win(&mut h, a);
    assert_eq!(approvals(&withdraw(&mut h, a, "30")), [(a, f("30"), 1)]);
    assert_eq!(h.engine.withdrawal_cash(), None);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V22 });
    assert_eq!(h.engine.withdrawal_cash(), Some(f("-10")));
    assert!(approvals(&withdraw(&mut h, a, "50")).is_empty());
    assert!(approvals(&deposit(&mut h, acct(2), "10")).is_empty());
    assert_eq!(approvals(&deposit(&mut h, acct(2), "15")), [(a, f("15"), 2)]);
    assert_eq!(h.engine.withdrawal_queue().next().unwrap().remaining, f("35"));
    assert_eq!(h.engine.withdrawal_cash(), Some(Fixed::ZERO));
    assert!(h.engine.withdrawal_approval(a, 1).is_none());
}

#[test]
fn earlier_versions_keep_fractional_withdrawals_and_omit_all_new_state() {
    let mut h = setup::predecessor();
    deposit(&mut h, acct(1), "1.000000999");
    assert_eq!(
        withdraw(&mut h, acct(1), "1.000000999"),
        [Event::WithdrawalApproved { account: acct(1), amount: f("1.000000999"), nonce: 1 }]
    );
    assert_eq!(h.balance(acct(1)), Fixed::ZERO);
    let state = serde_json::to_value(&h.engine).unwrap();
    for field in ["withdrawal_cash", "withdrawal_queue", "withdrawal_approvals", "withdrawal_next_ticket"] {
        assert!(state.get(field).is_none(), "legacy serialization must omit {field}");
    }
    assert!(h.engine.protocol_version() < ProtocolVersion::V22);
}

#[test]
fn rejected_or_duplicate_credits_never_create_payout_capacity() {
    let mut h = setup::two_waiting();
    let before = h.engine.withdrawal_queue().copied().collect::<Vec<_>>();
    let events = h.cmd(Command::Deposit { account: acct(3), amount: f("5"), chain_ref: "genesis".into() });
    assert!(events.iter().any(|e| matches!(e, Event::DepositRejected { reason: Reject::DuplicateReference, .. })));
    assert!(approvals(&events).is_empty());
    let events =
        h.cmd(Command::Deposit { account: acct(3), amount: Fixed::MAX_INPUT + Fixed::ONE, chain_ref: "bad".into() });
    assert!(events.iter().any(|e| matches!(e, Event::DepositRejected { reason: Reject::InvalidAmount, .. })));
    assert!(approvals(&events).is_empty());
    assert_eq!(h.engine.withdrawal_queue().copied().collect::<Vec<_>>(), before);
    assert_eq!(h.engine.withdrawal_cash(), Some(Fixed::ZERO));
}
