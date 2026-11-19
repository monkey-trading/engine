//! A member that cold-starts from an archive mirror whose copy is not the operator's log byte for
//! byte: one that spells seals otherwise gives it the operator's history, whose posted head its copy
//! holds while it follows with no rollback page; one that dropped a seal gives it a copy it refuses
//! to open, which pages `member_follow_blocked` at the copy, not `operator_log_rollback` at the
//! operator.

use super::*;
use crate::log::{LogEntry, MANIFEST, Manifest};
use crate::test_support::{rewrite_command, upper_case_seal};

/// The mirror spells the seals of the log's first command and of the last command it serves in upper
/// case. The member bootstraps from it, pins the operator's posted head at that last command, follows
/// the operator's stream, which repeats that command, and holds at its turn with no rollback page; its
/// copy commits to the operator's prefix at every seq.
#[tokio::test]
async fn a_member_cold_starting_from_a_mirror_that_spells_seals_otherwise_holds_the_operators_head() {
    let (_guard, mut alerts) = capture_alerts();
    let mut fixture = Fixture::start("member-copies-spelt", false).await;
    let last = remirror(&mut fixture, "member-copies-spelt-mirror", |dir, last| {
        rewrite_command(dir, 0, upper_case_seal);
        rewrite_command(dir, last, upper_case_seal);
    })
    .await;
    let operator = fixture.operator(false).await;
    fixture.registry.name(&operator.host);
    fixture.time.set(FIRST + 1);
    let held = crate::sequencer::replay_path(&fixture.log).unwrap().prefix_ring;
    *fixture.heads.0.lock().unwrap() = Some(Head { seq: last, link: held.prefix_at(last).unwrap() });
    let mut member = fixture.member();
    let drive = async {
        operator.wait_for_connections(2).await;
        fixture.seal_own_turn();
        fixture.time.set(OWN + 1);
    };
    let (turn, ()) = tokio::join!(member.until_turn(), drive);
    assert!(matches!(turn.unwrap(), Turn::Hold));
    let paged: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    assert!(paged.iter().all(|alert| alert["alert"] != "operator_log_rollback"), "an honest operator paged: {paged:?}");
    let copied = crate::sequencer::replay_path(&fixture.cfg.node.log_dir).unwrap().prefix_ring;
    let commands = log_entries(&fixture.cfg.node.log_dir).into_iter().filter_map(|entry| match entry {
        LogEntry::Command(command) => Some(command.sequenced.seq),
        _ => None,
    });
    for seq in commands {
        assert_eq!(copied.prefix_at(seq), held.prefix_at(seq), "the copy's link at seq {seq} is not the operator's");
    }
}

/// The mirror dropped the seal of the log's first command, which a later checkpoint covers: the member
/// refuses the copy it bootstrapped, and pages that it cannot open its copy, never that the operator
/// rolled its log back.
#[tokio::test]
async fn a_member_cold_starting_from_a_mirror_that_dropped_a_seal_refuses_the_copy_and_blames_no_operator() {
    let (_guard, mut alerts) = capture_alerts();
    let mut fixture = Fixture::start("member-copies-dropped", false).await;
    let last = remirror(&mut fixture, "member-copies-dropped-mirror", |dir, _| {
        rewrite_command(dir, 0, |command| command.seal = None);
    })
    .await;
    let operator = fixture.operator(false).await;
    fixture.registry.name(&operator.host);
    fixture.time.set(FIRST + 1);
    let held = crate::sequencer::replay_path(&fixture.log).unwrap().prefix_ring;
    *fixture.heads.0.lock().unwrap() = Some(Head { seq: last, link: held.prefix_at(last).unwrap() });
    let mut member = fixture.member();
    let mut paged = Vec::new();
    let blocked = tokio::select! {
        _ = member.until_turn() => panic!("the member stopped following"),
        blocked = crate::test_support::wait_for("the open_copy page", async {
            loop {
                let alert = alerts.recv().await.expect("the alert channel closed");
                if alert["alert"] == "member_follow_blocked" {
                    break alert;
                }
                paged.push(alert);
            }
        }) => blocked,
    };
    assert_eq!(blocked["step"], "open_copy", "{blocked:?}");
    paged.extend(std::iter::from_fn(|| alerts.try_recv().ok()));
    assert!(paged.iter().all(|alert| alert["alert"] != "operator_log_rollback"), "an honest operator paged: {paged:?}");
    assert_eq!(operator.connections(), 0, "the member followed onto a copy it refused");
}

/// The mirror dropped its segments' last checkpoint and sealed the commands after the one before it with
/// a key the vault never named, relisting the segments. The member pages `copy_tail_unaccepted` naming
/// that key, cuts the tail back to the checkpoint, the stream refills it, and nothing pages the operator:
/// no rollback, no reorg, and its copy commits to the operator's prefix at every seq.
#[tokio::test]
async fn a_member_cold_starting_from_a_mirror_that_forged_its_tail_pages_the_tail_not_the_operator() {
    let (_guard, mut alerts) = capture_alerts();
    let mut fixture = Fixture::start("member-copies-forged", false).await;
    let forger = EnclaveKey::generate();
    let last = remirror(&mut fixture, "member-copies-forged-mirror", |dir, _| {
        crate::test_support::drop_last_checkpoint(dir);
        let entries = log_entries(dir);
        let from = entries.iter().rposition(|entry| matches!(entry, LogEntry::Checkpoint(_))).unwrap();
        let mut link = match &entries[from] {
            LogEntry::Checkpoint(checkpoint) => crate::log::command_seal::checkpoint_link(checkpoint).unwrap(),
            _ => unreachable!(),
        };
        for entry in &entries[from..] {
            if let LogEntry::Command(command) = entry {
                let previous = link;
                link = crate::log::command_seal::command_link(previous, command).unwrap();
                let deployment = crate::log::CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };
                rewrite_command(dir, command.sequenced.seq, |command| {
                    command.seal =
                        Some(forger.seal_command(
                            deployment,
                            crate::log::command_seal::command_link(previous, command).unwrap(),
                        ));
                });
            }
        }
    })
    .await;
    let operator = fixture.operator(false).await;
    fixture.registry.name(&operator.host);
    fixture.time.set(FIRST + 1);
    let held = crate::sequencer::replay_path(&fixture.log).unwrap().prefix_ring;
    *fixture.heads.0.lock().unwrap() = Some(Head { seq: last, link: held.prefix_at(last).unwrap() });
    let mut member = fixture.member();
    let drive = async {
        operator.wait_for_connections(2).await;
        fixture.seal_own_turn();
        fixture.time.set(OWN + 1);
    };
    let (turn, ()) = tokio::join!(member.until_turn(), drive);
    assert!(matches!(turn.unwrap(), Turn::Hold));
    let paged: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    let blaming = ["operator_log_rollback", "promotion_reorged", "replica_diverged"];
    assert!(
        paged.iter().all(|alert| !blaming.contains(&alert["alert"].as_str())),
        "an honest operator paged: {paged:?}"
    );
    let forged = AccountId(forger.address()).to_string();
    assert!(
        paged.iter().any(|alert| alert["alert"] == "copy_tail_unaccepted" && alert["key"] == forged),
        "the forged tail paged nothing: {paged:?}"
    );
    let copied = crate::sequencer::replay_path(&fixture.cfg.node.log_dir).unwrap().prefix_ring;
    assert!(copied.prefix_at(last).is_some(), "the stream did not refill the tail the member cut");
    for seq in (0..=last).filter(|seq| copied.prefix_at(*seq).is_some()) {
        assert_eq!(copied.prefix_at(seq), held.prefix_at(seq), "the copy's link at seq {seq} is not the operator's");
    }
}

/// Points the member's only archive mirror at a new one, named for `name`, serving the completed
/// segments of the operator's log and their listings, changed by `alter`, which is given the last
/// command they hold: that seq.
async fn remirror(fixture: &mut Fixture, name: &str, alter: impl FnOnce(&std::path::Path, u64)) -> u64 {
    let mirror = scratch_dir(name);
    std::fs::create_dir_all(&mirror).unwrap();
    let listings = Manifest::load(&fixture.log).unwrap().segments;
    assert!(listings.len() >= 2, "the archive lists at least two completed segments");
    for listed in listings.iter() {
        std::fs::copy(fixture.log.join(&listed.file), mirror.join(&listed.file)).unwrap();
    }
    std::fs::copy(fixture.log.join(MANIFEST), mirror.join(MANIFEST)).unwrap();
    let last = listings.last().unwrap().last_seq;
    alter(&mirror, last);
    let url = archive(&mirror).await;
    fixture.cfg.tee.sealing = Some(
        toml::from_str(&format!(
            "dir = {:?}\n[handover]\nlisten = '127.0.0.1:0'\nmirrors = [{url:?}]\ndanger_accept_invalid_certs = true",
            fixture.keys.display().to_string()
        ))
        .unwrap(),
    );
    last
}
