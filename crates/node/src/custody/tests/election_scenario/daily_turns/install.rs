//! The governance fallback that ends the daily turns.

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::sol;
use alloy::sol_types::SolCall;
use engine_core::types::{AccountId, Millis};

use super::super::chain::{Chain, setRotationCall};
use super::super::{CONFIG_HASH, GOVERNANCE_DELAY_SECS};
use super::seat::Member;
use super::{FIRST_TURN, TURNS, boundary_secs, installOperatorCall};
use crate::checkpoints::{CheckpointChain, View};
use crate::tee::sealing::Release;

sol! {
    function setCurrentRelease(bytes launchDigest, bytes32 configHash) external;
    function scheduleBatch(address[] targets, uint256[] values, bytes[] payloads, bytes32 predecessor, bytes32 salt, uint256 delay) external;
    function executeBatch(address[] targets, uint256[] values, bytes[] payloads, bytes32 predecessor, bytes32 salt) external payable;
}

/// The launch measurement of the release the fallback installs, another of the last turn's candidate.
const FALLBACK_DIGEST: [u8; 48] = [0xd1; 48];

/// Turns rotation off, waits out the last drawn turn, and has the timelock install `member`, the last
/// turn's candidate, under another release and name that release, in one batch as the runbook does: once
/// finalised the schedule names no turn at any time, so neither side of a handover, nor a start, takes the
/// old turn's release and generation, and the vault names the installed release, folded.
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
        launchDigest: FALLBACK_DIGEST.to_vec().into(),
        configHash: CONFIG_HASH.into(),
    };
    let name = setCurrentReleaseCall { launchDigest: FALLBACK_DIGEST.to_vec().into(), configHash: CONFIG_HASH.into() };
    govern_batch(chain, vec![(chain.registry, install.abi_encode()), (chain.vault, name.abi_encode())]).await;
    chain.finalize().await;
    let at = vault.observe(View::Finalized, &[]).await.unwrap();
    assert_eq!(vault.scheduled_turn(&at, reached).await.unwrap(), None, "the installation ended the last turn");
    assert!(!crate::handover::turns::drawn(&vault, &at).await.unwrap(), "a start finds a turn drawn");
    let fallback = Release { launch_digest: FALLBACK_DIGEST, config_sha256: CONFIG_HASH }.id();
    assert_eq!(vault.current_release(&at).await.unwrap().id, fallback, "the vault names the installed release");
    assert_eq!(vault.synced_release(&at).await.unwrap(), fallback, "the naming folded the installed release");
    println!(
        "PASS: installing {}, the last turn's own candidate, under another release and naming it in one batch ends \
         the turns: the schedule names no one and the vault names the installed release",
        member.name
    );
}

/// Has the timelock make `calls` as one batch, in order: schedules it, moves the next block past the
/// delay, and executes it.
async fn govern_batch(chain: &Chain, calls: Vec<(Address, Vec<u8>)>) {
    let from = chain.accounts[0];
    let (targets, payloads): (Vec<Address>, Vec<Bytes>) = calls.into_iter().map(|(to, data)| (to, data.into())).unzip();
    let values = vec![U256::ZERO; targets.len()];
    let salt = keccak256("the fallback's installation and naming");
    let schedule = scheduleBatchCall {
        targets: targets.clone(),
        values: values.clone(),
        payloads: payloads.clone(),
        predecessor: B256::ZERO,
        salt,
        delay: U256::from(GOVERNANCE_DELAY_SECS),
    };
    chain.send(from, chain.governance, schedule.abi_encode()).await;
    chain.next_block_at(chain.latest_timestamp().await + GOVERNANCE_DELAY_SECS).await;
    let execute = executeBatchCall { targets, values, payloads, predecessor: B256::ZERO, salt };
    chain.send(from, chain.governance, execute.abi_encode()).await;
}
