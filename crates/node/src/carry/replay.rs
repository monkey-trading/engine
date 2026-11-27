//! `engine replay --carry-source`: checks a carried log against the published source log it carried from, which
//! replay alone cannot. The carried log proves each value against the root its genesis names; this ties that
//! root to the source log's signed checkpoint, and the carried values to the source's state there, the stakes a
//! `V93` genesis mirrors again included.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use engine_core::Engine;
use engine_core::carry::{CarriedAward, CarriedBalance, CarryPart, CarrySource};
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::protocol::ProtocolVersion;
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

/// What a carried log's replay shows of its carry: its parts, the protocol its genesis carried under, and from
/// `V93` the stakes once mirrored again, if the source had any.
struct Replayed {
    parts: Vec<CarryPart>,
    version: ProtocolVersion,
    remirrored: Option<BTreeMap<AccountId, TokenUnits>>,
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
/// source's state there, as `engine carry` builds it for the protocol the genesis carried under. From `V93` the
/// stake set the carried log holds once it has mirrored the stakes again is the source's there too.
/// Error cases:
/// - either log cannot be read, a carried command does not apply, or the carried log's genesis carries no state
/// - the source log holds no checkpoint at the carried seq, or its commands do not reproduce that root
/// - the checkpoint's root or deployment differs from the carried source, or its signature is not its signer's
/// - the source's state there cannot be carried, or carries other values than the carried log's genesis
/// - from `V93`, the carried log ends before it has mirrored the stakes again, or with another stake set
pub fn check_carried(carried: &Path, source: &Path) -> anyhow::Result<CarriedFrom> {
    let replayed = replay_carried(carried)?;
    let logged = flatten(&replayed.parts)?;
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
    let (parts, totals) = engine
        .carry_parts(replayed.version)
        .map_err(|refused| anyhow::anyhow!("the source cannot be carried: {refused}"))?;
    let rebuilt = flatten(&parts)?;
    if rebuilt != logged {
        bail!("the carried log's genesis carried other values than the source's state at seq {}", logged.source.seq);
    }
    if let Some(stakes) = &replayed.remirrored {
        ensure!(
            stakes == engine.staked_balances(),
            "the stakes the carried log mirrored again are not the source's at seq {}",
            logged.source.seq
        );
    }
    Ok(CarriedFrom { source: logged.source, signer, accounts: totals.accounts, awards: totals.awards })
}

/// The log at `path` replayed through its carried genesis: the carry parts it sequenced, in order, the protocol
/// they applied under, and from `V93` the stake set once its stakes are mirrored again, read no further.
/// Error cases:
/// - the log cannot be read or fails its manifest check, or a command does not apply
/// - its stakes are still being mirrored again where the log ends
fn replay_carried(path: &Path) -> anyhow::Result<Replayed> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    let mut engine = Engine::new();
    let (mut parts, mut version, mut remirrored) = (Vec::new(), ProtocolVersion::default(), None);
    let mut visit = |entry: LogEntry| {
        let LogEntry::Command(command) = entry else { return Ok(()) };
        let at = command.sequenced.seq;
        engine.apply(&command.sequenced).with_context(|| format!("command at seq {at} does not apply"))?;
        let ended = parts.last() == Some(&CarryPart::End);
        if let Command::Admin(AdminCommand::CarryState(part)) = command.sequenced.cmd
            && !ended
        {
            version = engine.protocol_version();
            parts.push(part);
        }
        if parts.last() == Some(&CarryPart::End) && !engine.remirroring() {
            remirrored = (version >= ProtocolVersion::V93).then(|| engine.staked_balances().clone());
            return Err(log::Stop.into());
        }
        Ok(())
    };
    let read =
        if found.is_dir() { SegmentedLog::open(path, &mut visit).map(drop) } else { log::read_file(path, &mut visit) };
    match read {
        Err(error) if !error.chain().any(|cause| cause.is::<log::Stop>()) => return Err(error),
        _ => {}
    }
    if parts.last() == Some(&CarryPart::End) {
        ensure!(
            remirrored.is_some() || version < ProtocolVersion::V93,
            "the carried log ends before its stakes are mirrored again"
        );
    }
    Ok(Replayed { parts, version, remirrored })
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
