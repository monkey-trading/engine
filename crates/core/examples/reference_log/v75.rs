//! The session's `V75` part: an epoch close waits for the staking mirror, counts a client's unfinalised
//! deposit as the client's own cash, and the operator key in effect signs nothing as a session key. A
//! vote activates the version, and the epochs its notice spanned wait for the mirror, which last
//! completed a scan in the `V73` part; once it catches up, the next command closes the latest of them.
//! A client parks a vault deposit a minute before the next epoch ends: the excess an epoch close would
//! pay is the same with it, and the close, held at the epoch's end until the mirror passes it, pays that
//! excess with the deposit still unfinalised. A client then registers the admin key of a member whose
//! turn is drawn, an hour before the turn's boundary; from the boundary that key's order for the client
//! is refused and spends no nonce, and its admin command applies on the nonce that was next.

use engine_core::Fixed;
use engine_core::access::receipt_access_deposit;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712::{self, B256};
use engine_core::event::{Event, Reject};
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::turns::{DAY_MS, RegistryTurn, TurnReason, boundary};
use engine_core::types::{Millis, Side};

use crate::common::close_spread::{attest, evidence, trade};
use crate::common::session_keys::trade_cmd_expiring;
use crate::common::{BTC, Client, ETH, Harness, f};
use crate::v47::LOSER_SEED;
use crate::v51::{MEMBER_SEEDS, MEMBERS};
use crate::v56::pass;

/// The seed of the client that parks a deposit and registers a session key.
const CLIENT_SEED: u8 = 0x76;
const MINUTE_MS: Millis = 60 * 1000;
const HOUR_MS: Millis = 60 * MINUTE_MS;
/// One USDC base unit in `Fixed`'s raw units.
const USDC: i128 = 1_000;

/// Activates `V75` by vote; then the held closes, the parked deposit and the operator's session key.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V75 };
    let events = pass(h, 180, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 180, command: activation })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V75);
    // The epochs the notice spanned were due at the activation, which holds them for the mirror.
    assert_eq!(closed(&events), None);
    // An activation leaves the rollovers its notice spanned to the next command.
    assert_eq!(closed(&h.cmd(Command::Tick)), None);
    frontier(h, h.ts);
    let events = h.cmd(Command::Tick);
    assert!(events.iter().any(|event| matches!(event, Event::DistributionEpochsSkipped { .. })), "{events:?}");
    assert!(closed(&events).is_some(), "{events:?}");

    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    held_close_with_a_parked_deposit(h, &client);
    operator_session_key(h);
}

/// The house earns, and a client parks 1,000 USDC a minute before the open epoch ends: the excess is
/// the same with the deposit. The close waits at the end for the mirror, and pays the excess with the
/// deposit still unfinalised; the deposit finalises after it.
fn held_close_with_a_parked_deposit(h: &mut Harness, client: &Client) {
    let config = h.engine.staking().expect("the session staked").config;
    let end = config.epoch_end(config.epochs_elapsed(h.ts));
    let loser = Client { expires_at: h.ts + 60_000, ..Client::new(LOSER_SEED) };
    for (side, price) in [(Side::Buy, "100"), (Side::Sell, "99.7")] {
        h.price(ETH, price);
        let events = trade(h, &loser, ETH, side, "1000", price);
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    }
    h.advance_to(end - MINUTE_MS);
    assert_eq!(closed(&h.cmd(Command::Tick)), None);
    let excess = h.engine.distributable_excess().expect("the session staked");
    assert!(excess >= Fixed::from_raw(USDC), "{excess}");
    let receipt = DepositReceipt {
        block: 76_000_000,
        block_hash: B256::repeat_byte(0x76),
        tx_hash: B256::repeat_byte(0x77),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 76_000_000,
        beneficiary: DepositBeneficiary::Trader { account: client.account },
        amount: f("1000"),
    };
    let deposit = receipt_access_deposit(&receipt, Some(client.account));
    let screened = evidence(h, eip712::deposit_screening_hash(&deposit), deposit.account, deposit.payer);
    let events =
        h.cmd(Command::AccessCustodyDeposit { receipt: receipt.clone(), payer: deposit.payer, evidence: screened });
    assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { .. })), "{events:?}");
    assert_eq!(h.engine.accounts()[&client.account].unfinalised_deposits, f("1000"));
    assert_eq!(h.engine.distributable_excess(), Some(excess));

    h.advance_to(end);
    assert_eq!(closed(&h.cmd(Command::Tick)), None);
    assert_eq!(closed(&frontier(h, end)), None);
    let at_close = h.engine.distributable_excess().expect("the session staked");
    let events = h.cmd(Command::Tick);
    let units = (at_close.raw() / USDC) as u128;
    assert!(units > 0 && closed(&events) == Some(units), "{at_close}: {events:?}");
    assert_eq!(h.engine.accounts()[&client.account].unfinalised_deposits, f("1000"));

    h.advance(5 * MINUTE_MS);
    let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).expect("recorded").updated_at_seq;
    let events = h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Finalise { receipt, observed_at_seq })));
    assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { .. })), "{events:?}");
}

/// A turn for `V51`'s first member is drawn from the latest set naming it. An hour before its boundary
/// the client registers the member's admin key as its session key; from the boundary, where the key is
/// the operator's, its order for the client is refused and spends no nonce, and the member takes the
/// turn and signs an admin command with the nonce that was next.
fn operator_session_key(h: &mut Harness) {
    let (member, seed) = (MEMBERS[0], MEMBER_SEEDS[0]);
    let admin_key = Client::new(seed).account;
    let set = h
        .engine
        .daily_turns()
        .sets
        .iter()
        .rev()
        .find(|(_, set)| set.members.iter().any(|m| m.candidate == member && m.admin_key == admin_key))
        .map(|(id, _)| *id)
        .expect("a mirrored set names the member");
    let turn = (0..).find(|turn| boundary(*turn).expect("in range") > h.ts + DAY_MS).expect("a later turn");
    let begins = boundary(turn).expect("in range");
    let drawn = RegistryTurn {
        turn,
        set,
        position: 0,
        reason: TurnReason::Draw,
        candidate: member,
        admin_key,
        block: 77_000_000,
        chain_ref: "0xturn:76:0".into(),
    };
    let events = h.cmd(Command::AssignTurn(drawn));
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");

    h.advance_to(begins - HOUR_MS);
    h.cmd(Command::Tick);
    assert_ne!(h.engine.operator().map(|operator| operator.address), Some(admin_key));
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    let nonce = h.engine.next_request_nonce(client.account);
    let expires_at = begins + 2 * HOUR_MS;
    let events = h.cmd(client.register_session_key(nonce, admin_key, expires_at));
    assert_eq!(events, [Event::SessionKeyRegistered { account: client.account, session_key: admin_key, expires_at }]);

    // The turn applies at the first command at its boundary.
    h.advance_to(begins);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(admin_key));
    let key = Client { expires_at: h.ts + 60_000, ..Client::new(seed) };
    let admin_nonce = h.engine.next_request_nonce(admin_key);
    let order = OrderRequest {
        account: client.account,
        client_id: 76,
        market: BTC,
        side: Side::Buy,
        qty: f("0.001"),
        kind: OrderKind::Limit { price: f("1") },
        reduce_only: false,
        auth: None,
    };
    let events = h.cmd(trade_cmd_expiring(h, &key, admin_nonce, order, key.expires_at));
    assert!(
        matches!(&events[..], [Event::OrderRejected { reason: Reject::InvalidSignature, .. }]),
        "the operator's key is no session key: {events:?}"
    );
    assert_eq!(h.engine.next_request_nonce(admin_key), admin_nonce);
    let release = B256::repeat_byte(76);
    let events = h.cmd(Command::TurnTaken { turn, release });
    assert_eq!(events.last(), Some(&Event::TurnTaken { turn, candidate: member, release }));
    let applied = h.operate_as(seed, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: false });
    assert_eq!(applied.last(), Some(&Event::AdminApplied), "{applied:?}");
}

/// The staking mirror completes a scan through a new block, stamped `time` on chain.
fn frontier(h: &mut Harness, time: Millis) -> Vec<Event> {
    let (block, block_hash) = (76_000_000 + h.log.len() as u64, B256::repeat_byte(0x76));
    let events = h.cmd(Command::StakingProgress { block, block_hash, block_time: Some(time), named_turn: None });
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    events
}

/// The excess the close among `events` measured, in USDC base units, if an epoch closed.
fn closed(events: &[Event]) -> Option<u128> {
    events.iter().find_map(|event| match event {
        Event::DistributionEpochClosed { excess, .. } => Some(excess.0),
        _ => None,
    })
}
