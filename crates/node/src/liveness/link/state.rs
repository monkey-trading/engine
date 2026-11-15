//! How a member's copy records the heads it saw against its own links: a conflict, a second history on
//! the head's key, or the copy's own divergence.

use engine_core::types::AccountId;
use tracing::error;

use crate::liveness::link::{CopyState, SEEN_EPOCHS, SignerEpoch};

impl CopyState {
    /// Signer epoch `epoch` of the enclave the copy follows.
    pub(super) fn under(&self, epoch: u64) -> SignerEpoch {
        (epoch, self.following.unwrap_or(AccountId([0; 20])))
    }

    /// Records that the copy's link at `seq` differs from a head of `epoch`: a conflict when the head's
    /// key sealed the copy's command there, else the copy's own divergence, which a rebuild drops.
    pub(super) fn mismatch(&mut self, epoch: SignerEpoch, seq: u64) {
        match self.links.get(&seq).and_then(|(_, sealer)| *sealer) {
            Some(sealer) if sealer == epoch.1 => self.conflict(epoch, seq),
            _ => {
                self.diverged.entry(epoch).or_insert(seq);
                while self.diverged.len() > SEEN_EPOCHS {
                    self.diverged.pop_first();
                }
            }
        }
    }

    /// Records that the copy holds another link at `seq` than a head of `epoch` it saw: a second
    /// history under the key that signed the head. Critical once per epoch.
    pub(super) fn conflict(&mut self, epoch: SignerEpoch, seq: u64) {
        if let std::collections::btree_map::Entry::Vacant(first) = self.conflicted.entry(epoch) {
            first.insert(seq);
            let (epoch, key) = epoch;
            error!(
                alert = "liveness_member_conflict",
                epoch,
                %key,
                seq,
                "this member's copy holds another link at seq {seq} than a signed head of signer epoch {epoch}: a \
                 second history on the operator's key; this member confirms nothing under that epoch"
            );
        }
        while self.conflicted.len() > SEEN_EPOCHS {
            self.conflicted.pop_first();
        }
    }
}
