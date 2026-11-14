//! What the daily turns scenario reads to check each turn: the open epoch's time in office, the
//! operator view, and the commands of a stopped node's log.

use std::collections::BTreeMap;
use std::path::Path;

use axum::http::StatusCode;
use engine_core::command::{Command, Sequenced};
use engine_core::types::{AccountId, Millis};
use serde_json::Value;

use super::super::node::Node;
use crate::log::LogEntry;

/// Each account's time in office in the open distribution epoch, and the last sequenced time.
pub(super) async fn office(node: &Node) -> (BTreeMap<AccountId, Millis>, Millis) {
    node.query(|e| {
        let office = e.staking().and_then(|staking| staking.office.as_ref()).expect("V50 counts time in office");
        (office.served.clone(), e.now())
    })
    .await
}

/// `served` with `by` more for `account`.
pub(super) fn credited(
    mut served: BTreeMap<AccountId, Millis>,
    account: AccountId,
    by: Millis,
) -> BTreeMap<AccountId, Millis> {
    assert!(by >= 0, "credit {by} is negative");
    if by > 0 {
        *served.entry(account).or_default() += by;
    }
    served
}

/// `GET /v1/operator`, which must answer.
pub(super) async fn operator_view(node: &Node) -> Value {
    let (status, view) = node.get("/v1/operator").await;
    assert_eq!(status, StatusCode::OK, "{view}");
    view
}

/// Every command in the log at `dir`, in seq order, from its segment files.
pub(super) fn logged_commands(dir: &Path) -> Vec<Sequenced> {
    let mut segments: Vec<(u64, String)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().into_string().ok()?;
            crate::log::segment_seq(&name).map(|first| (first, name))
        })
        .collect();
    segments.sort();
    let mut commands = Vec::new();
    for (_, name) in segments {
        crate::log::read_file(&dir.join(name), |entry| {
            if let LogEntry::Command(command) = entry {
                commands.push(command.sequenced);
            }
            Ok(())
        })
        .unwrap();
    }
    assert!(commands.iter().enumerate().all(|(i, c)| c.seq == i as u64), "the log's commands are not contiguous");
    commands
}

/// The seq and stamp of the `TurnTaken` for `turn` in `commands`, which must hold exactly one.
pub(super) fn turn_taken(commands: &[Sequenced], turn: u64) -> (u64, Millis) {
    let taken: Vec<_> = commands
        .iter()
        .filter(|c| matches!(c.cmd, Command::TurnTaken { turn: taken, .. } if taken == turn))
        .map(|c| (c.seq, c.ts))
        .collect();
    assert_eq!(taken.len(), 1, "turn {turn} is taken once: {taken:?}");
    taken[0]
}

/// The seq of the last command in the log at `dir`.
pub(super) fn last_seq(dir: &Path) -> u64 {
    logged_commands(dir).last().expect("the log holds commands").seq
}

/// Asserts that the copy at `copy` holds the commands of the log at `log` through seq `through`, as
/// the log holds them.
pub(super) fn is_prefix(copy: &Path, log: &Path, through: u64) {
    let (copied, logged) = (logged_commands(copy), logged_commands(log));
    let through = usize::try_from(through).unwrap() + 1;
    assert!(copied.len() >= through, "the copy ends at seq {}, before {}", copied.len(), through - 1);
    let as_json = |commands: &[Sequenced]| serde_json::to_value(commands).unwrap();
    assert_eq!(as_json(&copied[..through]), as_json(&logged[..through]), "the copy differs from the log");
}
