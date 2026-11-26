//! A skip or promotion the schedule refuses while the newest head's release belongs to no candidate: its
//! revert, `KeyHolderUnknown`, and the release it names (`OperatorSchedule`, `LogHeads`).

use alloy::primitives::B256;
use alloy::sol;
use alloy::sol_types::{SolCall, SolError};
use anyhow::Context;

use crate::liveness::chain::LivenessDomain;
use crate::standby_key::chain::AgreedBlock;

sol! {
    function headRelease(uint64 epoch) external view returns (bytes32);

    /// `OperatorSchedule`'s refusal of a skip or promotion while the newest head's release belongs to no candidate.
    error KeyHolderUnknown();
}

/// A skip or promotion the schedule refused with `KeyHolderUnknown`: the newest head's release belongs to no
/// candidate, so neither can pass until a member takes the keys or the release is bound to its candidate.
#[derive(Debug)]
pub(crate) struct KeyHolderRefused;

impl std::fmt::Display for KeyHolderRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the schedule refused the call: the newest head's release belongs to no candidate")
    }
}

impl std::error::Error for KeyHolderRefused {}

/// Whether the revert in `error`, as the endpoint reports it, carries `KeyHolderUnknown`'s selector.
pub(crate) fn names_unknown_key_holder(error: &str) -> bool {
    error.to_ascii_lowercase().contains(&alloy::hex::encode(KeyHolderUnknown::SELECTOR))
}

/// The release of the signer epoch's newest v2 head, which `domain`'s `LogHeads` records.
/// Error cases:
/// - the call fails, or its return does not decode
pub(crate) async fn head_release(
    block: &impl AgreedBlock,
    domain: &LivenessDomain,
    epoch: u64,
) -> anyhow::Result<B256> {
    let request = headReleaseCall { epoch };
    let out = block.call("liveness_head_release", domain.log_heads, request.abi_encode().into()).await?;
    headReleaseCall::abi_decode_returns(&out).context("decoding liveness_head_release")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_key_holder_revert_is_taken_for_an_unowned_release() {
        let selector = alloy::hex::encode(KeyHolderUnknown::SELECTOR);
        let refused = format!(
            "liveness RPC failed: server returned an error response: error code 3: execution reverted, data: \"0x{selector}\""
        );
        assert!(names_unknown_key_holder(&refused));
        assert!(!names_unknown_key_holder(
            "liveness RPC failed: error code 3: execution reverted, data: \"0x4e487b71\""
        ));
        assert!(!names_unknown_key_holder("liveness RPC failed: execution reverted: not due"));
    }
}
