//! The session's `V85` part: one screening decision for an account. A vote activates the version. A
//! client declares, buys BTC, sells it and withdraws on one decision of the source's that names no
//! request, each request at its own nonce and all within the recorded bound; its deposit is screened on
//! a decision naming the deposit. A decision naming the order it was signed for still backs that order,
//! but not the next one; another account's decision and the client's own once the bound has passed are
//! refused, and a refused decision refuses the client's next order and withdrawal while it is fresh.

use engine_core::Fixed;
use engine_core::access::{
    AccessEvidence, AccessReject, EligibilityClass, EligibilityRequest, Jurisdiction, SanctionsStatus, Screening,
};
use engine_core::auth::RequestAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, WithdrawalRequest};
use engine_core::eip712::{self, B256, Signature};
use engine_core::event::{Event, Reject};
use engine_core::fixed::Rounding;
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis, Side};

use crate::common::close_spread::{fund, source, source_key};
use crate::common::events::only_fill;
use crate::common::{BTC, Client, Harness, f};
use crate::v56::pass;

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x85;
/// The first of this part's proposal ids.
const PROPOSAL: u64 = 270;
/// The freshness bound the session records (`v24`).
const MAX_AGE_MS: Millis = 30_000;

/// Activates `V85` by vote; then the decisions described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V85 };
    let events = pass(h, PROPOSAL, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: PROPOSAL, command: activation })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V85);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);
    let btc = h.engine.markets()[&BTC].mark.expect("traded before");
    h.price(BTC, &btc.to_string());

    // One decision for the client, observed now, backs each of its requests within the bound.
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    let decision = screened(h, decision_for(h, client.account));
    let events = h.cmd(Command::AttestEligibility { request: declaration(h, &client), evidence: decision.clone() });
    let attested = Event::EligibilityAttested { account: client.account, class: EligibilityClass::Professional };
    assert_eq!(events, [attested]);
    fund(h, &client, "20000");
    for side in [Side::Buy, Side::Sell] {
        h.advance(10_000);
        h.price(BTC, &btc.to_string());
        only_fill(&h.cmd(order(h, &client, side, btc, decision.clone()).0));
    }
    let events = h.cmd(withdrawal(h, &client, decision.clone()).0);
    assert!(events.iter().any(|event| matches!(event, Event::WithdrawalApproved { .. })), "{events:?}");

    // A decision naming the request it answers still backs it; one naming another request, another
    // account's, and the client's own once past the bound do not.
    let (request, hash) = order(h, &client, Side::Buy, btc, decision.clone());
    let named = screened(h, Screening { request_hash: Some(hash), ..decision_for(h, client.account) });
    let Command::AccessOrder { request, .. } = request else { unreachable!() };
    only_fill(&h.cmd(Command::AccessOrder { request, evidence: named.clone() }));
    let events = h.cmd(order(h, &client, Side::Sell, btc, named).0);
    assert_eq!(refusal(&events), Some(AccessReject::EvidenceMismatch), "{events:?}");
    let other = screened(h, decision_for(h, AccountId([CLIENT_SEED; 20])));
    let events = h.cmd(order(h, &client, Side::Buy, btc, other).0);
    assert_eq!(refusal(&events), Some(AccessReject::EvidenceMismatch), "{events:?}");
    h.advance(MAX_AGE_MS - 20_000 + 1);
    h.price(BTC, &btc.to_string());
    let events = h.cmd(order(h, &client, Side::Buy, btc, decision).0);
    assert_eq!(refusal(&events), Some(AccessReject::EvidenceStale), "{events:?}");

    // A refused decision refuses each request it backs.
    let refused =
        screened(h, Screening { jurisdiction: Jurisdiction::UnitedStates, ..decision_for(h, client.account) });
    let events = h.cmd(order(h, &client, Side::Buy, btc, refused.clone()).0);
    assert_eq!(refusal(&events), Some(AccessReject::UnitedStates), "{events:?}");
    let events = h.cmd(withdrawal(h, &client, refused).0);
    assert_eq!(refusal(&events), Some(AccessReject::UnitedStates), "{events:?}");
}

/// The source's allowed and clear decision for `account`, observed now and naming no request, unsigned.
fn decision_for(h: &Harness, account: AccountId) -> Screening {
    Screening {
        source: source_key(),
        request_hash: None,
        observed_at: h.ts,
        account: Some(account),
        payer: None,
        jurisdiction: Jurisdiction::Allowed,
        account_status: SanctionsStatus::Clear,
        payer_status: SanctionsStatus::Clear,
        signature: Signature::ZERO,
    }
}

/// `screening` signed by the recorded source.
fn screened(h: &Harness, mut screening: Screening) -> AccessEvidence {
    let domain = h.engine.access_domain().expect("a recorded domain");
    screening.signature = eip712::sign(&source(), &eip712::screening_hash(&domain, &screening));
    AccessEvidence::Screened(screening)
}

/// The client's professional declaration at its next nonce.
fn declaration(h: &Harness, client: &Client) -> EligibilityRequest {
    let (nonce, expires_at) = (h.engine.next_request_nonce(client.account), h.ts + 60_000);
    let class = EligibilityClass::Professional;
    let hash = eip712::eligibility_hash(&client.domain, client.account, class, nonce, expires_at);
    EligibilityRequest {
        account: client.account,
        class,
        auth: RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) },
    }
}

/// The client's market order for $1,000 of BTC on `side`, quoted at `quoted`, at its next nonce with
/// `evidence`, and the digest it signs.
fn order(h: &Harness, client: &Client, side: Side, quoted: Fixed, evidence: AccessEvidence) -> (Command, B256) {
    let (nonce, expires_at) = (h.engine.next_request_nonce(client.account), h.ts + 60_000);
    let qty = Fixed::from_int(1_000).div_rounded(quoted, Rounding::Down);
    let mut request = OrderRequest {
        account: client.account,
        client_id: 82,
        market: BTC,
        side,
        qty: qty - Fixed::from_raw(qty.raw() % 1_000_000),
        kind: OrderKind::Market { quoted, max_slippage_bps: None },
        reduce_only: false,
        auth: None,
        take_profit: None,
        stop_loss: None,
    };
    let hash = eip712::order_hash(&client.domain, &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) });
    (Command::AccessOrder { request, evidence }, hash)
}

/// The client's withdrawal of 1,000 at its next nonce with `evidence`, and the digest it signs.
fn withdrawal(h: &Harness, client: &Client, evidence: AccessEvidence) -> (Command, B256) {
    let (nonce, expires_at) = (h.engine.next_request_nonce(client.account), h.ts + 60_000);
    let amount = f("1000");
    let hash = eip712::withdrawal_hash(&client.domain, client.account, amount, nonce, expires_at);
    let auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) });
    let request = WithdrawalRequest { account: client.account, amount, auth };
    (Command::AccessWithdraw { request, evidence, payout_available: true }, hash)
}

/// The access refusal `events` hold, if any.
fn refusal(events: &[Event]) -> Option<AccessReject> {
    events.iter().find_map(|event| match event {
        Event::OrderRejected { reason: Reject::Access { reason }, .. }
        | Event::WithdrawalRejected { reason: Reject::Access { reason }, .. } => Some(*reason),
        _ => None,
    })
}
