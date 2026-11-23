//! The session's `V88` part: a receipt above the finalised custody frontier at a vault id the frontier
//! counts, price rules that would leave a held market with no price, and the operator key signing for
//! its own address. A vote activates the version, and another, passed beside it, relists BTC under rules
//! no venue set meets. The custody watcher then credits a client's deposit in a block above the
//! frontier the `V20` part recorded, at vault id 0, which that frontier counts as a finalised deposit's;
//! the engine refuses it, where before `V88` it took it as a second receipt at the id. The operator makes
//! BTC, which holds positions, reduce-only, and the voted relisting is refused, where before `V88`
//! reduce-only let it through. The key in office then signs an order for its own address, which is
//! refused and spends no nonce. The version's notice rules apply only to the operator's own changes,
//! which votes replace here, and its election rule only to a registry with no rotation sets, which this
//! session has, so `tests/notice_window.rs`, `tests/futures_contract_bounds.rs` and
//! `tests/election_order.rs` cover them.

use alloy_primitives::B256;
use engine_core::access::receipt_access_deposit;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositReject};
use engine_core::eip712;
use engine_core::event::{Event, Reject};
use engine_core::governance::VotedChange;
use engine_core::market::{MarketSpec, PriceRules};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::close_spread::{attest, evidence};
use crate::common::session_keys::trade_cmd_expiring;
use crate::common::{BTC, Client, Harness, f};
use crate::v76::office_seed;
use crate::v81::{applied, scheduled};

/// The seed of the key this part deposits through.
const CLIENT_SEED: u8 = 0x88;
/// The first of this part's proposal ids.
const PROPOSAL: u64 = 292;

/// Activates `V88` by vote, with the voted relisting beside it; then the refusals above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V88 };
    scheduled(h, PROPOSAL, &activation);
    let btc = h.engine.baseline_spec(BTC).expect("BTC is listed");
    let rules = h.engine.markets()[&BTC].spec.price_rules;
    let unmeetable = PriceRules { min_sources: 9, ..btc.price_rules };
    let relisting = AdminCommand::UpsertMarket(MarketSpec { price_rules: unmeetable, ..btc });
    scheduled(h, PROPOSAL + 1, &relisting);
    h.advance(NOTICE_MS);
    applied(h, PROPOSAL, activation);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V88);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);

    let frontier = h.engine.custody_checkpoint().expect("the `V20` part recorded a frontier");
    assert!(frontier.deposit_count > 0, "{frontier:?}");
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    let stray = DepositReceipt {
        block: frontier.block + 8_800_000,
        block_hash: B256::repeat_byte(0x88),
        tx_hash: B256::repeat_byte(0x89),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 0,
        beneficiary: DepositBeneficiary::Trader { account: client.account },
        amount: f("1000"),
    };
    let deposit = receipt_access_deposit(&stray, Some(client.account));
    let evidence = evidence(h, eip712::deposit_screening_hash(&deposit), deposit.account, deposit.payer);
    let events = h.cmd(Command::AccessCustodyDeposit { receipt: stray.clone(), payer: Some(client.account), evidence });
    let refused = Event::CustodyDepositRejected { receipt: stray.clone(), reason: DepositReject::ConflictingReceipt };
    assert!(events.contains(&refused), "{events:?}");
    assert!(h.engine.recorded_deposit(&stray.reference()).is_none());

    // BTC holds positions: reduce-only no longer lets rules through that would leave it with no price.
    let seed = office_seed(h);
    let reduce_only = h.operate_as(seed, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true });
    assert_eq!(reduce_only.last(), Some(&Event::AdminApplied), "{reduce_only:?}");
    let voted = VotedChange { proposal: PROPOSAL + 1, command: relisting };
    let events = h.cmd(Command::ApplyProposal(Box::new(voted)));
    let refused =
        matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("would have no price"));
    assert!(refused, "{events:?}");
    assert_eq!(h.engine.markets()[&BTC].spec.price_rules, rules, "the refused relisting changes nothing");
    let resumed = h.operate_as(seed, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: false });
    assert_eq!(resumed.last(), Some(&Event::AdminApplied), "{resumed:?}");

    // The key in office signs no order for its own address, and spends no nonce of the admin channel's.
    let office = Client { expires_at: h.ts + 60_000, ..Client::new(seed) };
    let nonce = h.engine.next_request_nonce(office.account);
    let order = OrderRequest {
        account: office.account,
        client_id: 88,
        market: BTC,
        side: Side::Buy,
        qty: f("0.001"),
        kind: OrderKind::Limit { price: f("1") },
        reduce_only: false,
        take_profit: None,
        stop_loss: None,
        auth: None,
    };
    let events = h.cmd(trade_cmd_expiring(h, &office, nonce, order, office.expires_at));
    assert!(matches!(&events[..], [Event::OrderRejected { reason: Reject::InvalidSignature, .. }]), "{events:?}");
    assert_eq!(h.engine.next_request_nonce(office.account), nonce);
}
