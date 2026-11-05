//! The operator registry's mirrorable events, as `contracts/src/OperatorRegistry.sol` declares
//! them, and its schedule's, as `contracts/src/OperatorSchedule.sol` does, decoded for the staking
//! mirror, which reads them in the same proven blocks as the staking contract's.

use alloy::primitives::B256;
use alloy::rpc::types::Log;
use alloy::sol;
use alloy::sol_types::SolEvent;
use anyhow::{Context, bail, ensure};
use engine_core::emissions::TokenUnits;
use engine_core::turns::{SetMember, TurnReason};
use engine_core::types::AccountId;

use crate::custody::evm::StakingEventKind;

sol! {
    event RotationChanged(bool enabled, bool electionCancelled);
    event Elected(uint64 indexed epoch, address indexed candidate, address adminKey, bytes32 release, uint64 effectiveAt);
    event OperatorInstalled(address indexed candidate, address indexed adminKey, bytes32 indexed release);
    event RotationSetAdopted(
        uint32 indexed setId,
        uint64 indexed epoch,
        uint64 effectiveAt,
        bool quorum,
        address[] candidates,
        address[] adminKeys,
        uint256[] weights
    );
    event TurnAssigned(
        uint64 indexed turn,
        uint32 indexed setId,
        uint16 position,
        uint8 reason,
        address candidate,
        address adminKey,
        bytes32 release,
        uint64 generation
    );
}

/// The topics of the registry events the engine mirrors.
pub(super) const SIGNATURES: [B256; 3] =
    [RotationChanged::SIGNATURE_HASH, Elected::SIGNATURE_HASH, OperatorInstalled::SIGNATURE_HASH];

/// The engine's view of a registry log: a rotation switch, an election with its effective time
/// in milliseconds, or an installation the timelock made.
/// Error cases:
/// - the log is none of the events, or does not decode
/// - an effective time whose milliseconds pass `i64`
pub(super) fn decode(log: &Log) -> anyhow::Result<StakingEventKind> {
    match log.topic0() {
        Some(&RotationChanged::SIGNATURE_HASH) => {
            let event = RotationChanged::decode_log(&log.inner).context("decoding RotationChanged")?.data;
            Ok(StakingEventKind::RotationChanged {
                enabled: event.enabled,
                election_cancelled: event.electionCancelled,
            })
        }
        Some(&Elected::SIGNATURE_HASH) => {
            let event = Elected::decode_log(&log.inner).context("decoding Elected")?.data;
            let effective_at = i64::try_from(event.effectiveAt)
                .ok()
                .and_then(|seconds| seconds.checked_mul(1000))
                .with_context(|| format!("election effective time {} is out of range", event.effectiveAt))?;
            Ok(StakingEventKind::Elected {
                epoch: event.epoch,
                candidate: AccountId(event.candidate.into()),
                admin_key: AccountId(event.adminKey.into()),
                effective_at,
            })
        }
        Some(&OperatorInstalled::SIGNATURE_HASH) => {
            let event = OperatorInstalled::decode_log(&log.inner).context("decoding OperatorInstalled")?.data;
            Ok(StakingEventKind::Installed {
                candidate: AccountId(event.candidate.into()),
                admin_key: AccountId(event.adminKey.into()),
            })
        }
        other => bail!("unexpected registry event {other:?}"),
    }
}

/// The topics of the schedule events the engine mirrors.
pub(super) const SCHEDULE_SIGNATURES: [B256; 2] = [RotationSetAdopted::SIGNATURE_HASH, TurnAssigned::SIGNATURE_HASH];

/// The engine's view of a schedule log: a rotation set adopted, or a turn assigned.
/// Error cases:
/// - the log is neither event, or does not decode
/// - a set's member lists differ in length, or a weight or an effective time is out of range
/// - a turn's reason is not one the schedule names
pub(super) fn decode_schedule(log: &Log) -> anyhow::Result<StakingEventKind> {
    match log.topic0() {
        Some(&RotationSetAdopted::SIGNATURE_HASH) => {
            let event = RotationSetAdopted::decode_log(&log.inner).context("decoding RotationSetAdopted")?.data;
            ensure!(
                event.candidates.len() == event.adminKeys.len() && event.candidates.len() == event.weights.len(),
                "a rotation set's member lists differ in length"
            );
            let members = (event.candidates.iter().zip(&event.adminKeys).zip(&event.weights))
                .map(|((candidate, admin_key), weight)| {
                    let weight =
                        u128::try_from(*weight).with_context(|| format!("set weight {weight} exceeds u128"))?;
                    Ok(SetMember {
                        candidate: AccountId(candidate.0.0),
                        admin_key: AccountId(admin_key.0.0),
                        weight: TokenUnits(weight),
                    })
                })
                .collect::<anyhow::Result<_>>()?;
            Ok(StakingEventKind::RotationSetAdopted {
                set: event.setId,
                epoch: event.epoch,
                effective_at: millis(event.effectiveAt).context("rotation set effective time")?,
                quorum: event.quorum,
                members,
            })
        }
        Some(&TurnAssigned::SIGNATURE_HASH) => {
            let event = TurnAssigned::decode_log(&log.inner).context("decoding TurnAssigned")?.data;
            let reason = match event.reason {
                0 => TurnReason::Draw,
                1 => TurnReason::Skip,
                2 => TurnReason::Promotion,
                other => bail!("turn {} has unknown reason {other}", event.turn),
            };
            Ok(StakingEventKind::TurnAssigned {
                turn: event.turn,
                set: event.setId,
                position: event.position,
                reason,
                candidate: AccountId(event.candidate.0.0),
                admin_key: AccountId(event.adminKey.0.0),
            })
        }
        other => bail!("unexpected schedule event {other:?}"),
    }
}

/// `seconds` as milliseconds, if they fit `i64`.
fn millis(seconds: u64) -> Option<engine_core::types::Millis> {
    i64::try_from(seconds).ok().and_then(|seconds| seconds.checked_mul(1000))
}
