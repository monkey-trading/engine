//! How the operator weighs the members' verified statuses against its log.

use std::collections::BTreeSet;

use alloy::primitives::B256;
use engine_core::types::AccountId;

use crate::liveness::Status;
use crate::liveness::operator::Assessment;
use crate::log_heads::Head;

/// Weighs verified `statuses` against this log, whose cumulative prefix at a seq `prefix_at` gives
/// while it is retained, and whose tip is `tip`: a member's durable copy, and the newest head it has
/// seen, each past the tip, at a seq where this log holds another link, or matching. A member whose
/// copy or seen head conflicts with this log is no confirmation. A durable copy whose last command
/// another key than this enclave's (`own`) sealed, from a member in `held` — one that held an
/// assignment of the turn, whose tail that key is its own holding's: a failed operator's tail its
/// promotion cut off, restarted as a member — is no copy of this key: it neither conflicts nor holds
/// seqs this log lacks. Any other member's copy counts, whichever key sealed it: a member that followed
/// an outgoing operator holds what it confirmed.
pub(crate) fn assess(
    statuses: &[(AccountId, Status)],
    prefix_at: impl Fn(u64) -> Option<B256>,
    tip: Head,
    (own, held): (AccountId, &BTreeSet<AccountId>),
) -> Assessment {
    let mut out = Assessment { answered: statuses.len(), ..Default::default() };
    for &(member, status) in statuses {
        let mut conflicts = false;
        let foreign = status.sealer != AccountId([0; 20]) && status.sealer != own && held.contains(&member);
        let durable = (!foreign).then_some(status.durable);
        for head in [durable, status.seen].into_iter().flatten() {
            if head.seq > tip.seq {
                out.beyond = out.beyond.max(Some((head.seq, member)));
            } else if prefix_at(head.seq).is_some_and(|link| link != head.link) {
                out.conflict = out.conflict.or(Some((head.seq, member)));
                conflicts = true;
            }
        }
        // A member that holds, or has seen, another history at a seq of this log confirms nothing,
        // whatever its durable copy matches.
        let durable = status.durable;
        if !conflicts && durable.seq <= tip.seq && prefix_at(durable.seq).is_some_and(|link| link == durable.link) {
            out.confirmed = out.confirmed.max(Some((durable.seq, member)));
        }
    }
    out
}
