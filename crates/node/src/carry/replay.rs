//! `engine replay --carry-source`: checks a carried log against the published source log it carried from, which
//! replay alone cannot. The carried log proves each value against the root its genesis names; this ties that
//! root to the source log's signed checkpoint, and the carried values to the source's state there.

use std::fs;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use engine_core::carry::{CarriedAward, CarriedBalance, CarryPart, CarrySource};
use engine_core::command::{AdminCommand, Command};
use engine_core::types::AccountId;

use crate::log::{self, CheckpointDomain, LogEntry, SegmentedLog};
use crate::standby::chain::admit_checkpoint;

/// What a carried log's genesis carried, batching aside: its source, each entitlement and stake by account,
/// and each award as the source held it.
#[derive(Debug, PartialEq, Eq)]
struct Carried {
    source: CarrySource,
    entitlements: Vec<(AccountId, u128)>,
    stakes: Vec<(AccountId, u128)>,
    awards: Vec<(u64, engine_core::awards::Award)>,
}

/// What `check_carried` found: the source checkpoint the carried log's genesis took its state from, and the
/// enclave whose signature over it the source log holds.
#[derive(Debug)]
pub struct CarriedFrom {
    pub source: CarrySource,
    pub signer: AccountId,
    pub accounts: u64,
    pub awards: u64,
}

/// Checks that the carried log at `carried` took its genesis's state from the source log at `source`: the source
/// log holds a checkpoint at the carried seq whose root its commands reproduce and is the carried root, signed
/// over the carried chain and vault by the enclave the entry names, and every value the genesis carried is the
/// source's state there, as `engine carry` builds it.
/// Error cases:
/// - either log cannot be read, or the carried log's genesis carries no state
/// - the source log holds no checkpoint at the carried seq, or its commands do not reproduce that root
/// - the checkpoint's root or deployment differs from the carried source, or its signature is not its signer's
/// - the source's state there cannot be carried, or carries other values than the carried log's genesis
pub fn check_carried(carried: &Path, source: &Path) -> anyhow::Result<CarriedFrom> {
    let logged = flatten(&carried_parts(carried)?)?;
    let (engine, checkpoint) = crate::exit::engine_and_checkpoint(source, logged.source.seq)?;
    let root = format!("{}", logged.source.root);
    ensure!(
        checkpoint.root == root,
        "the source's checkpoint at seq {} holds root {}, not the carried {root}",
        checkpoint.seq,
        checkpoint.root
    );
    let domain = CheckpointDomain { chain_id: logged.source.chain_id, vault: logged.source.vault };
    ensure!(
        checkpoint.domain.is_none_or(|signed| signed == domain),
        "the source's checkpoint at seq {} is signed for {:?}, not the carried source's deployment",
        checkpoint.seq,
        checkpoint.domain
    );
    let signer = admit_checkpoint(&checkpoint, &domain).context("the source's checkpoint at the carried seq")?;
    let (parts, totals) =
        engine.carry_parts().map_err(|refused| anyhow::anyhow!("the source cannot be carried: {refused}"))?;
    let rebuilt = flatten(&parts)?;
    if rebuilt != logged {
        bail!("the carried log's genesis carried other values than the source's state at seq {}", logged.source.seq);
    }
    Ok(CarriedFrom { source: logged.source, signer, accounts: totals.accounts, awards: totals.awards })
}

/// The carry parts the log at `path`'s genesis sequenced, in order, read up to its `End`.
/// Error cases:
/// - the log cannot be read or fails its manifest check
fn carried_parts(path: &Path) -> anyhow::Result<Vec<CarryPart>> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    let mut parts = Vec::new();
    let mut visit = |entry: LogEntry| {
        if let LogEntry::Command(command) = entry
            && let Command::Admin(AdminCommand::CarryState(part)) = command.sequenced.cmd
        {
            let end = part == CarryPart::End;
            parts.push(part);
            if end {
                return Err(log::Stop.into());
            }
        }
        Ok(())
    };
    let read =
        if found.is_dir() { SegmentedLog::open(path, &mut visit).map(drop) } else { log::read_file(path, &mut visit) };
    match read {
        Err(error) if !error.chain().any(|cause| cause.is::<log::Stop>()) => return Err(error),
        _ => {}
    }
    Ok(parts)
}

/// What `parts` carry, batching aside.
/// Error cases:
/// - no `Begin` first, or no `End` last
fn flatten(parts: &[CarryPart]) -> anyhow::Result<Carried> {
    let Some(CarryPart::Begin { source, .. }) = parts.first() else { bail!("the log's genesis carries no state") };
    ensure!(parts.last() == Some(&CarryPart::End), "the log's carried genesis does not end");
    let balances =
        |entries: &[CarriedBalance]| entries.iter().map(|entry| (entry.account, entry.amount.0)).collect::<Vec<_>>();
    let mut carried = Carried { source: *source, entitlements: Vec::new(), stakes: Vec::new(), awards: Vec::new() };
    for part in parts {
        match part {
            CarryPart::Entitlements { entries } => carried.entitlements.extend(balances(entries)),
            CarryPart::Stakes { entries } => carried.stakes.extend(balances(entries)),
            CarryPart::Awards { awards, .. } => {
                carried.awards.extend(awards.iter().map(|CarriedAward { id, award, .. }| (*id, award.clone())));
            }
            CarryPart::Begin { .. } | CarryPart::End => {}
        }
    }
    Ok(carried)
}
