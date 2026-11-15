//! The governance fallback that ends the daily turns.

use alloy::primitives::Address;
use alloy::sol_types::SolCall;
use engine_core::types::{AccountId, Millis};

use super::super::chain::{Chain, setRotationCall};
use super::seat::Member;
use super::{FIRST_TURN, TURNS, boundary_secs, installOperatorCall};
use crate::checkpoints::{CheckpointChain, View};

/// Turns rotation off, waits out the last drawn turn, and has the timelock install `member`, the last
/// turn's candidate, under another release: once finalised the schedule names no turn at any time,
/// so neither side of a handover, nor a start, takes the old turn's release and generation.
pub(super) async fn installation_ends_the_turns(chain: &mut Chain, member: &Member) {
    let vault = crate::handover::vault_reader(&member.config(chain)).unwrap().expect("[checkpoints] reads the vault");
    chain.govern_each(vec![(chain.registry, setRotationCall { enabled: false }.abi_encode())]).await;
    let ended = boundary_secs(FIRST_TURN + TURNS);
    chain.mine_at((ended + 60).max(chain.latest_timestamp().await + 1)).await;
    chain.finalize().await;
    let reached = (chain.timestamp_at("finalized").await as Millis) * 1000;
    let at = vault.observe(View::Finalized, &[]).await.unwrap();
    let over = vault.scheduled_turn(&at, reached).await.unwrap().expect("a turn over still names its member");
    assert_eq!((over.turn, over.candidate), (FIRST_TURN + TURNS - 1, AccountId(member.candidate.into())));
    let install = installOperatorCall {
        candidate: member.candidate,
        adminKey: Address::from(member.operator().0),
        launchDigest: [0xd1u8; 48].to_vec().into(),
        configHash: super::super::CONFIG_HASH.into(),
    };
    chain.govern_each(vec![(chain.registry, install.abi_encode())]).await;
    chain.finalize().await;
    let at = vault.observe(View::Finalized, &[]).await.unwrap();
    assert_eq!(vault.scheduled_turn(&at, reached).await.unwrap(), None, "the installation ended the last turn");
    assert!(!crate::handover::turns::drawn(&vault, &at).await.unwrap(), "a start finds a turn drawn");
    println!(
        "PASS: installing {}, the last turn's own candidate, under another release ends the turns: the schedule names no one",
        member.name
    );
}
