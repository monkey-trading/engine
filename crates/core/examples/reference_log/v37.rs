//! The session's `V37` part: the operator key follows the operator registry's mirrored elections.

use engine_core::command::{AdminCommand, Command};
use engine_core::election::RegistryElection;
use engine_core::event::Event;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};

use crate::common::{Client, Harness};

/// The seed of the key `V35`'s part rotated the operator to.
const OPERATOR_SEED: u8 = 0x55;
/// The seed of the elected candidate's admin key.
const ELECTED_SEED: u8 = 0x56;
const CANDIDATE: AccountId = AccountId([0xc7; 20]);
const HOUR_MS: Millis = 60 * 60 * 1000;

/// `V37` activates. An election before rotation is on is refused, the registry turns rotation on
/// (a repeat of its reference is refused), and a signed rotation is refused while it is on. An
/// election logged a day before its effective time waits seven days, and turning rotation off
/// cancels it. Rotation turns on again, and the next election takes effect once its time has come
/// and the staking mirror's finalised frontier has reached it: the operator's key is refused from
/// then, and the elected key applies.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V37 });
    let elected = Client::new(ELECTED_SEED).account;
    let refused = |events: &[Event]| {
        assert!(
            matches!(events.last(), Some(Event::ElectionRejected { .. } | Event::AdminRejected { .. })),
            "{events:?}"
        );
    };
    let mut block = 37_000_000;
    let mut mirror = |h: &mut Harness, cmd: fn(u64, String) -> Command, reference: &str| {
        block += 1;
        h.cmd(cmd(block, reference.into()))
    };
    let on = |block, chain_ref| Command::SwitchRotation { enabled: true, election_cancelled: false, block, chain_ref };
    let cancel =
        |block, chain_ref| Command::SwitchRotation { enabled: false, election_cancelled: true, block, chain_ref };

    let early = RegistryElection {
        epoch: 1,
        candidate: CANDIDATE,
        admin_key: elected,
        effective_at: h.ts + NOTICE_MS,
        block: 37_000_000,
        chain_ref: "0xelect:37:0".into(),
    };
    refused(&h.cmd(Command::ElectOperator(early.clone())));
    mirror(h, on, "0xrotation:37:0");
    refused(&mirror(h, on, "0xrotation:37:0"));
    refused(&h.operate_as(OPERATOR_SEED, AdminCommand::RotateOperator { address: elected }));

    // Logged a day before it takes effect on chain, the election waits seven days; turning rotation
    // off cancels it first.
    let deferred = RegistryElection { effective_at: h.ts + 24 * HOUR_MS, block: 37_000_010, ..early };
    let events = h.cmd(Command::ElectOperator(deferred));
    assert!(
        matches!(events.last(), Some(Event::OperatorElected { effective_at, .. }) if *effective_at == h.ts + NOTICE_MS),
        "{events:?}"
    );
    h.advance(HOUR_MS);
    mirror(h, cancel, "0xrotation:37:1");
    assert_eq!(h.engine.operator_bounds().rotation, None);
    mirror(h, on, "0xrotation:37:2");

    let effective_at = h.ts + NOTICE_MS + HOUR_MS;
    let election =
        RegistryElection { epoch: 2, effective_at, block: 37_000_020, chain_ref: "0xelect:37:1".into(), ..early };
    h.cmd(Command::ElectOperator(election));
    h.advance_to(effective_at - 1);
    let reduce_only = |reduce_only| AdminCommand::SetReduceOnly { market: None, reduce_only };
    assert_eq!(h.operate_as(OPERATOR_SEED, reduce_only(true)).last(), Some(&Event::AdminApplied));
    // The key waits until the staking mirror's finalised frontier reaches the registry's time.
    h.advance_to(effective_at);
    assert_eq!(h.operate_as(OPERATOR_SEED, reduce_only(true)).last(), Some(&Event::AdminApplied));
    let frontier = Command::StakingProgress {
        block: 37_000_030,
        block_hash: alloy_primitives::B256::repeat_byte(37),
        block_time: Some(effective_at),
    };
    assert!(h.cmd(frontier).is_empty());
    refused(&h.operate_as(OPERATOR_SEED, reduce_only(false)));
    assert_eq!(h.operate_as(ELECTED_SEED, reduce_only(false)).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(elected));
}
