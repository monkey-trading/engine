//! The signed-withdrawal check replay runs before each command.

use std::collections::VecDeque;

use anyhow::bail;
use engine_core::Fixed;
use engine_core::types::AccountId;

/// Checks the last command's approved withdrawals all have their signed entries.
/// Error cases:
/// - `must_sign` and an approval is still unmatched
pub(super) fn ensure_signed(
    must_sign: bool,
    approved: &VecDeque<(AccountId, Fixed, u64)>,
    seq: Option<u64>,
) -> anyhow::Result<()> {
    match approved.front() {
        Some((account, _, nonce)) if must_sign => {
            bail!("withdrawal {account} nonce {nonce} approved at seq {seq:?} has no signed entry")
        }
        _ => Ok(()),
    }
}
