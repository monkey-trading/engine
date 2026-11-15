//! A promoted member's gap fetch: what the members of the turn's set hold past its copy, then the
//! outgoing operator's archive, all within one budget, so a member with a complete copy starts well
//! inside the detection window however many members are unreachable. A source whose entries the copy
//! refuses is passed over from then on, and the follower, which a refused entry may have left ahead of
//! the copy, is opened again before any other source is asked.

use std::collections::BTreeSet;
use std::time::Duration;

use alloy::primitives::Address;
use anyhow::Context;
use engine_core::types::AccountId;
use futures_util::future::join_all;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::liveness::chain::Member as SetMember;
use crate::liveness::link::{Asking, Reply, Request, Tcp, describe};
use crate::log::Offset;
use crate::node::member::promoted::missing;
use crate::node::member::standby::Standby;
use crate::standby::Follower;
use crate::standby_key::chain::Promotion;

/// How long the whole gap fetch may take, members and archive together.
pub(super) const BUDGET: Duration = Duration::from_secs(60);
/// How long one member gets to answer a round.
const ASK: Duration = Duration::from_secs(5);
/// How many rounds of `Entries` answers the fetch takes at most.
const ROUNDS: usize = 64;

/// A source of the gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Source {
    Member(AccountId),
    Archive,
}

/// The copy refused what a source gave: the follower must be opened again.
#[derive(Debug)]
struct TailRefused;

impl std::fmt::Display for TailRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the copy refused the archive's entries")
    }
}

impl std::error::Error for TailRefused {}

/// Fetches what the members of `promotion`'s turn's set hold past the copy, the member whose enclave
/// confirmed the outgoing epoch's head first, as the operator's stream named it; then, while the copy
/// still lacks a prefix the promotion fixed, what the outgoing `operator`'s archive holds; before
/// `deadline`, asking no source in `refused`. Returns the first source whose entries the copy refused,
/// after which the follower must be opened again.
pub(super) async fn fetch(
    standby: &Standby,
    promotion: &Promotion,
    follower: &mut Follower,
    operator: Option<&crate::standby::Followed>,
    (deadline, refused): (Instant, &BTreeSet<Source>),
) -> Option<Source> {
    let mut members = match tokio::time::timeout_at(deadline, standby.turn_members()).await {
        Ok(Ok(members)) => members,
        Ok(Err(e)) => {
            warn!("reading the turn's members for the gap failed: {e:#}");
            Vec::new()
        }
        Err(_) => Vec::new(),
    };
    let confirmer = standby
        .copy()
        .signed()
        .filter(|signed| signed.head.head.seq == promotion.head_seq)
        .map(|signed| signed.head.confirmer);
    members.retain(|member| {
        member.candidate != standby.candidate()
            && !member.endpoint.is_empty()
            && !refused.contains(&Source::Member(member.candidate))
    });
    members.sort_by_key(|member| Some(Address::from(member.candidate.0)) != confirmer);
    let (taken, refusal) = from_members(&Tcp, &members, follower, deadline).await;
    info!(taken, members = members.len(), "asked the turn's members for the gap");
    if let Some(member) = refusal {
        return Some(Source::Member(member));
    }
    let archive_due = !refused.contains(&Source::Archive) && missing(follower.replica(), promotion).is_some();
    if let Some(operator) = operator.filter(|_| archive_due) {
        match tokio::time::timeout_at(deadline, archive(follower, operator)).await {
            Ok(Ok(taken)) => info!(taken, "took the gap from the outgoing operator's archive"),
            Ok(Err(e)) if e.downcast_ref::<TailRefused>().is_some() => {
                warn!("the copy refused the outgoing operator's archive: {e:#}");
                return Some(Source::Archive);
            }
            Ok(Err(e)) => warn!("the outgoing operator's archive did not fill the gap: {e:#}"),
            Err(_) => warn!("the gap fetch's budget of {BUDGET:?} ran out before the archive answered"),
        }
    }
    None
}

/// Takes what `members` hold past the copy: each round asks every member at once, each within `ASK`
/// and all before `deadline`, and takes the first answer, in `members`' order, that extends the copy;
/// rounds go on while one does. Returns how many entries the copy took, and the member whose entries
/// the copy refused, if one did: the fetch stops there, since the follower must be opened again.
pub(super) async fn from_members(
    asking: &impl Asking,
    members: &[SetMember],
    follower: &mut Follower,
    deadline: Instant,
) -> (u64, Option<AccountId>) {
    let mut taken = 0;
    for _ in 0..ROUNDS {
        // From the copy's last entry, past its tip's command when a page ended among the entries
        // that follow it, so the next page goes on from there.
        let Some(after) = follower.head() else { return (taken, None) };
        let request = Request::Entries { after };
        let by = deadline.min(Instant::now() + ASK);
        let answers =
            join_all(members.iter().map(|member| tokio::time::timeout_at(by, asking.ask(&member.endpoint, &request))))
                .await;
        let mut progressed = false;
        for (member, answer) in members.iter().zip(answers) {
            let entries = match answer {
                Ok(Ok(Reply::Entries { entries })) if !entries.is_empty() => entries,
                Ok(Ok(other)) => {
                    info!(member = %member.candidate, "the member has no entries past {after}: {}", describe(&other));
                    continue;
                }
                Ok(Err(e)) => {
                    warn!(member = %member.candidate, "asking a member for the gap failed: {e:#}");
                    continue;
                }
                Err(_) => {
                    warn!(member = %member.candidate, "the member did not answer for the gap in time");
                    continue;
                }
            };
            let mut lines = Vec::new();
            for (_, entry) in &entries {
                match serde_json::to_vec(entry) {
                    Ok(line) => {
                        lines.extend(line);
                        lines.push(b'\n');
                    }
                    Err(e) => {
                        warn!("encoding a fetched entry failed: {e:#}");
                        return (taken, None);
                    }
                }
            }
            match follower.take_tail(after, &lines) {
                Ok(count) if count > 0 => {
                    info!(member = %member.candidate, taken = count, %after, "took the gap from a member");
                    (taken, progressed) = (taken + count, true);
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(member = %member.candidate, "a member's entries do not extend the copy: {e:#}");
                    return (taken, Some(member.candidate));
                }
            }
        }
        if !progressed || Instant::now() >= deadline {
            return (taken, None);
        }
    }
    (taken, None)
}

/// Takes what `operator`'s archive and its mirrors hold past the copy's tip: every entry of the
/// completed segments after the command at the tip, through the copy's seal chain as a handover's tail.
/// Returns how many entries the copy took.
/// Error cases:
/// - the archive cannot be read, a segment differs from its listing or does not parse, or its entries
///   do not extend the copy
async fn archive(follower: &mut Follower, operator: &crate::standby::Followed) -> anyhow::Result<u64> {
    use crate::standby::archive::{archive_client, check_listing, fetch_archive_bytes, fetch_manifest};
    let (archive, origins) = (archive_client(operator.danger_accept_invalid_certs)?, operator.origins());
    let tip = follower.replica().last_seq().context("the copy holds no command")?;
    let listings = fetch_manifest(&archive, &origins).await?;
    let mut lines = Vec::new();
    for listed in listings.iter().filter(|listed| listed.last_seq >= tip) {
        let bytes = fetch_archive_bytes(&archive, &origins, &listed.file, Some(listed)).await?;
        check_listing(listed, &bytes)?;
        let mut seq = listed.first_seq.saturating_sub(1);
        for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
            let entry: crate::log::LogEntry = serde_json::from_slice(line).context("an archived entry")?;
            if let crate::log::LogEntry::Command(command) = &entry {
                seq = command.sequenced.seq;
            }
            let after_tip = match entry {
                crate::log::LogEntry::Command(_) => seq > tip,
                _ => seq >= tip,
            };
            if after_tip {
                lines.extend_from_slice(line);
                lines.push(b'\n');
            }
        }
    }
    anyhow::ensure!(!lines.is_empty(), "the archive holds nothing past seq {tip}");
    follower.take_tail(Offset::start_of(tip), &lines).map_err(|e| e.context(TailRefused))
}

#[cfg(test)]
mod tests;
