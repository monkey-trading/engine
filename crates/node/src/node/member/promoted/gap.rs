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
/// still lacks a prefix the promotion fixed, what the outgoing `operator`'s archive holds, and what the
/// members hold past that, the recent entries they keep going on from its completed segments; before
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
            Ok(Ok(taken)) => {
                info!(taken, "took the gap from the outgoing operator's archive");
                // The archive serves completed segments: the members' recent entries go on from there.
                if taken > 0 && missing(follower.replica(), promotion).is_some() {
                    let (taken, refusal) = from_members(&Tcp, &members, follower, deadline).await;
                    info!(taken, "asked the turn's members again past the archive");
                    if let Some(member) = refusal {
                        return Some(Source::Member(member));
                    }
                }
            }
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
    // Rounds go on while one extends the copy and the deadline allows: a whole log is many pages.
    loop {
        // From the copy's last entry, past its tip's command when a page ended among the entries
        // that follow it, so the next page goes on from there; from the log's first for an empty copy.
        let after = follower.head();
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
                    info!(member = %member.candidate, ?after, "the member has no entries past it: {}", describe(&other));
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
            match follower.take_entries(after, &lines) {
                Ok(count) if count > 0 => {
                    info!(member = %member.candidate, taken = count, ?after, "took the gap from a member");
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
}

/// Takes what `operator`'s archive and its mirrors hold past the copy's tip, or all of it for an empty
/// copy: every entry of the completed segments after the command at the tip, a segment at a time through
/// the copy's seal chain as a handover's tail.
/// Returns how many entries the copy took.
/// Error cases:
/// - the archive cannot be read, a segment differs from its listing or does not parse, or its entries
///   do not extend the copy
async fn archive(follower: &mut Follower, operator: &crate::standby::Followed) -> anyhow::Result<u64> {
    use crate::standby::archive::{archive_client, check_listing, fetch_archive_bytes, fetch_manifest};
    let (archive, origins) = (archive_client(operator.danger_accept_invalid_certs)?, operator.origins());
    let listings = fetch_manifest(&archive, &origins).await?;
    let mut taken = 0;
    // One segment at a time: memory holds one, and what is taken stays taken if a later one fails.
    for listed in &listings {
        let tip = follower.replica().last_seq();
        if tip.is_some_and(|tip| listed.last_seq < tip) {
            continue;
        }
        let step = async {
            let bytes = fetch_archive_bytes(&archive, &origins, &listed.file, Some(listed)).await?;
            check_listing(listed, &bytes)?;
            let mut lines = Vec::new();
            past_tip(&bytes, listed.first_seq, tip, &mut lines)?;
            if lines.is_empty() {
                return anyhow::Ok(0);
            }
            follower.take_entries(tip.map(Offset::start_of), &lines).map_err(|e| e.context(TailRefused))
        };
        match step.await {
            Ok(count) => taken += count,
            Err(e) if taken > 0 && e.downcast_ref::<TailRefused>().is_none() => {
                warn!("the archive's later segments were not taken: {e:#}");
                break;
            }
            Err(e) => return Err(e),
        }
    }
    anyhow::ensure!(taken > 0, "the archive holds nothing past the copy's tip");
    Ok(taken)
}

/// Appends to `lines` the JSON lines of the segment `bytes`, which starts at `first_seq`, that follow the
/// command at `tip`: its later commands, and the entries at its seq or later; every line for an empty
/// copy, whose `tip` is `None`.
/// Error cases:
/// - a line is not a log entry
fn past_tip(bytes: &[u8], first_seq: u64, tip: Option<u64>, lines: &mut Vec<u8>) -> anyhow::Result<()> {
    let mut seq = first_seq.saturating_sub(1);
    for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
        let entry: crate::log::LogEntry = serde_json::from_slice(line).context("an archived entry")?;
        if let crate::log::LogEntry::Command(command) = &entry {
            seq = command.sequenced.seq;
        }
        let after_tip = match entry {
            crate::log::LogEntry::Command(_) => tip.is_none_or(|tip| seq > tip),
            _ => tip.is_none_or(|tip| seq >= tip),
        };
        if after_tip {
            lines.extend_from_slice(line);
            lines.push(b'\n');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
