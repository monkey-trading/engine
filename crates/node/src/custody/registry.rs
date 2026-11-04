//! The operator registry's mirrorable events, as `contracts/src/OperatorRegistry.sol` declares
//! them, decoded for the staking mirror, which reads them in the same proven blocks as the staking
//! contract's.

use alloy::primitives::B256;
use alloy::rpc::types::Log;
use alloy::sol;
use alloy::sol_types::SolEvent;
use anyhow::{Context, bail};
use engine_core::types::AccountId;

use crate::custody::evm::StakingEventKind;

sol! {
    event RotationChanged(bool enabled, bool electionCancelled);
    event Elected(uint64 indexed epoch, address indexed candidate, address adminKey, bytes32 release, uint64 effectiveAt);
    event OperatorInstalled(address indexed candidate, address indexed adminKey, bytes32 indexed release);
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
