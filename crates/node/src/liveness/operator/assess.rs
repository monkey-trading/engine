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
/// an outgoing operator holds what it confirmed. A member whose durable copy ends at a seq of this log
/// whose prefix is no longer retained, where no conflict can be seen, counts as neither answered nor
/// empty, so the start check passes only on another member's answer. A seen head at such a seq is
/// neither: an hourly head, or one a poster outage left old, would otherwise hold the check for good.
pub(crate) fn assess(
    statuses: &[(AccountId, Status)],
    prefix_at: impl Fn(u64) -> Option<B256>,
    tip: Head,
    (own, held): (AccountId, &BTreeSet<AccountId>),
) -> Assessment {
    let mut out = Assessment::default();
    for &(member, status) in statuses {
        let mut conflicts = false;
        let foreign = status.sealer != AccountId([0; 20]) && status.sealer != own && held.contains(&member);
        let durable = status.durable.filter(|_| !foreign);
        if !durable.is_some_and(|head| head.seq <= tip.seq && prefix_at(head.seq).is_none()) {
            match status.durable {
                Some(_) => out.answered += 1,
                None => out.empty += 1,
            }
        }
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
        if let Some(durable) = status.durable
            && !conflicts
            && durable.seq <= tip.seq
            && prefix_at(durable.seq).is_some_and(|link| link == durable.link)
        {
            out.confirmed = out.confirmed.max(Some((durable.seq, member)));
        }
    }
    out
}
