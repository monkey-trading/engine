//! What the vault's exit against the last posted root checks, from `V36`, through `Engine::apply`:
//! the leaves it proves, an account proof's equity at the root's marks, and the proofs an account
//! exits with, which follow the committed root rather than the live state.

#[allow(dead_code)]
#[path = "access/support.rs"]
mod access_support;
mod common;
#[path = "exit/custody.rs"]
mod custody;
#[path = "exit/fixture.rs"]
mod fixture;
#[path = "exit/support.rs"]
mod support;

use alloy_primitives::B256;
use common::{BTC, DE40, USDJPY, acct, f};
use engine_core::Fixed;
use engine_core::access::{AccessDeposit, AccessPolicy};
use engine_core::command::{AdminCommand, Command};
use engine_core::eip712;
use engine_core::merkle::{
    AccountProof, ExitTerms, PendingReceipt, exit_terms_hash, exit_terms_key, pending_receipt_hash,
    withdrawal_queue_hash, withdrawal_queue_key,
};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};
use engine_core::withdrawal::QueuedWithdrawal;
use support::{fill, positioned, trading};

#[test]
fn the_exit_leaves_hash_as_an_evm_toolchain_encodes_them() {
    // Each expected hash is `cast keccak` over `cast abi-encode` of the module docs' layout.
    let queued = QueuedWithdrawal {
        account: AccountId([1; 20]),
        amount: f("110"),
        remaining: f("80"),
        requested_seq: 42,
        ticket: 3,
    };
    assert_eq!(
        withdrawal_queue_hash(&queued, ProtocolVersion::V36),
        "0x28fc9968390f1c3ba6fd06646d0cedf422cf0b53be302377516e3a4e4441e83f".parse::<B256>().unwrap()
    );
    let json = alloy_primitives::keccak256(serde_json::to_vec(&queued).unwrap());
    assert_eq!(withdrawal_queue_hash(&queued, ProtocolVersion::V35), json, "earlier roots keep the JSON leaf");
    assert_eq!(
        withdrawal_queue_key(42),
        "0x6a8c48defe56e7f63df69794da7955aa508559fb67a7f5fc7435db1d12e5dfb3".parse::<B256>().unwrap()
    );
    let terms = ExitTerms {
        deposit_cursor: 3,
        deposit_base: 0,
        pending: f("40"),
        held: f("30"),
        unheld: f("4"),
        held_receipts: f("2"),
        gaps: vec![],
    };
    assert_eq!(
        exit_terms_hash(&terms),
        "0xb26d0b5933c815750a41640985551a2bc768a8526d3f168099f8e6a80ff37b35".parse::<B256>().unwrap()
    );
    let gap = ExitTerms { gaps: vec![5, 7], ..terms };
    assert_eq!(
        exit_terms_hash(&gap),
        "0xad481e019f29e0fef75b0be0ff29dee109ad868e2332dd9037db49d3a3a67e23".parse::<B256>().unwrap()
    );
    assert_eq!(
        exit_terms_key(),
        "0xf1ad5ac184f0821d8f121f0029e00f46ee673269e94fd876972913229f7570ab".parse::<B256>().unwrap()
    );
    let trader = PendingReceipt {
        deposit_id: 2,
        account: Some(AccountId([2; 20])),
        payer: Some(AccountId([3; 20])),
        amount: f("40"),
    };
    assert_eq!(
        pending_receipt_hash(&trader),
        "0x0b32c687b3a7c14572ddc0f2e4a7c7596075f11570e1690bc9b380d838d52a17".parse::<B256>().unwrap()
    );
    let house = PendingReceipt { deposit_id: 5, account: None, payer: Some(AccountId([3; 20])), amount: f("7") };
    assert_eq!(
        pending_receipt_hash(&house),
        "0x63c86acb24846b492546be243d551e5c5e8964aecbeb159986125f88953defdb".parse::<B256>().unwrap()
    );
}

#[test]
fn an_account_proof_values_every_position_as_the_engines_equity_does() {
    // Gains and losses, long and short, in USD, a euro market and a yen market.
    let moves: [(&str, &str, &str); 4] =
        [("21000", "20500", "151"), ("19000", "19500", "149"), ("20100", "20000", "150"), ("30000", "25000", "100")];
    for (btc, de40, usdjpy) in moves {
        let (mut h, a, _) = trading();
        h.price(BTC, btc);
        h.price(DE40, de40);
        h.price(USDJPY, usdjpy);
        h.engine.state_root();
        let proof = h.engine.account_proof(a).unwrap();
        assert!(proof.verify(proof.root));
        assert_eq!(proof.positions.len(), 3);
        assert_eq!(proof.equity(), Some(h.equity_and_margin(a).0), "BTC {btc}, DE40 {de40}, USD-JPY {usdjpy}");
    }
}

#[test]
fn equity_rounds_each_positions_pnl_down_before_and_after_converting_it() {
    let (mut h, a, _) = trading();
    // A loss of one raw unit a coin on a fractional BTC position, and losses of a third of a yen
    // and of a third of a euro cent a unit, leave negative fractions in both steps, where rounding
    // down and toward zero part.
    let entry = h.engine.accounts()[&a].positions[&BTC].entry;
    h.price(BTC, &(entry - Fixed::from_raw(1)).to_string());
    h.price(USDJPY, "149.666666667");
    h.price(DE40, "20000.003333333");
    h.engine.state_root();
    let proof = h.engine.account_proof(a).unwrap();
    let equity = proof.equity().unwrap();
    assert_eq!(equity, h.equity_and_margin(a).0);
    let truncated = AccountProof { positions: proof.positions[..2].to_vec(), ..proof.clone() };
    assert!(!truncated.verify(proof.root), "an account proof missing a position does not verify");
}

#[test]
fn a_market_without_an_fx_mark_has_no_equity_unless_its_pnl_is_zero() {
    let (mut h, a, _) = trading();
    h.engine.state_root();
    let mut proof = h.engine.account_proof(a).unwrap();
    let euro = proof.markets.iter().position(|m| m.market == DE40).unwrap();
    proof.markets[euro].leaf.fx_mark = None;
    assert_eq!(proof.equity(), None, "a euro PnL with no FX mark cannot be valued");
    let held = proof.positions.iter().position(|p| p.market == DE40).unwrap();
    proof.markets[euro].leaf.mark = Some(proof.positions[held].position.entry);
    assert!(proof.equity().is_some(), "a zero PnL needs no FX mark");
}

#[test]
fn a_shortfall_proves_its_approval_and_queue_against_the_root_that_committed_them() {
    let (mut h, a, queued_seq) = trading();
    let root = B256::from(h.engine.state_root());
    let proof = h.engine.exit_proof(a).unwrap();
    assert_eq!((proof.root, &proof.terms.terms), (root, &ExitTerms::default()));
    assert!(proof.terms.verify(root));
    let [approval] = &proof.approvals[..] else { panic!("one approval: {:?}", proof.approvals) };
    assert!(approval.verify(root));
    let [queued] = &proof.queued[..] else { panic!("one queued request: {:?}", proof.queued) };
    assert_eq!(queued.queued.requested_seq, queued_seq);
    assert_eq!(approval.approval.amount + queued.queued.remaining, f("150000"));
    assert!(queued.verify(root));
    assert_eq!(proof.account.as_ref().unwrap().positions.len(), 3);
    assert!(proof.distribution.is_none() && proof.pending_deposits.is_empty());

    // A screened house deposit approves more of the head; until the next root, the proofs stay
    // against this one.
    let source = eip712::address(access_support::source().verifying_key());
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source, max_age_ms: 1_000 }));
    let deposit =
        AccessDeposit { account: None, payer: Some(acct(9)), amount: f("20000"), chain_ref: "0xfunded:9".into() };
    let evidence = access_support::evidence(&h, eip712::deposit_screening_hash(&deposit), None, Some(acct(9)));
    let events = h.cmd(Command::AccessDeposit { deposit, evidence });
    assert!(events.iter().any(|e| matches!(e, engine_core::event::Event::HouseCapitalMoved { .. })), "{events:?}");
    let live = h.engine.withdrawal_queue().next().unwrap().remaining;
    assert!(live < queued.queued.remaining);
    assert_eq!(h.engine.exit_proof(a).unwrap(), proof);
    let next = B256::from(h.engine.state_root());
    let later = h.engine.exit_proof(a).unwrap();
    assert_eq!((later.approvals.len(), later.queued[0].queued.remaining), (2, live));
    assert!(later.queued[0].verify(next) && !queued.verify(next));
}

#[test]
fn activating_v36_rebuilds_the_tree_under_the_exit_encodings() {
    let (mut h, a, _) = support::trading_at(ProtocolVersion::V35);
    let before = h.engine.state_root();
    assert!(h.engine.exit_proof(a).is_none(), "no exit terms before V36");
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V36 });
    let after = h.engine.state_root();
    assert_ne!(before, after);
    let mut rebuilt: engine_core::Engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
    assert_eq!(rebuilt.state_root(), after, "the incremental root is a rebuild's");
    let proof = h.engine.exit_proof(a).unwrap();
    assert!(proof.queued[0].verify(B256::from(after)) && proof.terms.verify(B256::from(after)));
}

#[test]
fn a_flat_account_values_at_its_balance_and_one_with_losses_beyond_it_at_below_zero() {
    let (mut h, a) = positioned();
    for (market, side, qty, mid) in [(BTC, Side::Sell, "1.333333333", "21000"), (DE40, Side::Buy, "2", "20000")] {
        h.price(market, mid);
        fill(&h.market_order(a, market, side, qty, mid));
    }
    h.price(USDJPY, "150");
    fill(&h.market_order(a, USDJPY, Side::Sell, "1000", "150"));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V36 });
    h.engine.state_root();
    let proof = h.engine.account_proof(a).unwrap();
    assert!(proof.positions.is_empty() && proof.verify(proof.root));
    assert_eq!(proof.equity(), Some(h.balance(a)));
    let mut losing = proof.clone();
    losing.leaf.balance = -Fixed::ONE;
    assert_eq!(losing.equity(), Some(-Fixed::ONE), "equity can be negative; the vault floors the claim");
}
