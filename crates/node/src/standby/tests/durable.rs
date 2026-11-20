//! A member's copy records as durable only what it appended, with the key that sealed it, and its
//! replica keeps the stats worker in step with what it applies.

use std::time::Duration;

use super::*;

/// A synced stint records the last command the copy appended as its durable head, with the key that
/// sealed it, which a member's status then names: not the replica's head, which runs ahead of the copy
/// when an append fails.
#[tokio::test]
async fn a_stint_records_what_the_copy_appended_as_durable_with_its_sealer() {
    let operator = scratch_dir("standby-durable-operator");
    ticks(&operator, 2).await;
    let dir = scratch_dir("standby-durable-copy");
    copy_log(&operator, &dir);
    ticks(&operator, 2).await;
    let mut follower = Follower::open(&dir, 1024, DOMAIN, enclave(), [enclave()].into(), None, |_| {}).unwrap();
    let link = crate::liveness::link::Copy::default();
    follower.link_with(link.clone());
    let (tip, _) = follower.tip().unwrap();
    let frames = log_messages(&operator, Offset::start_of(tip)).into_iter().map(|message| (By::Operator, message));
    let (port, server, _) = operator_stream(frames.collect()).await;
    let followed = Followed::from_standby(&standby_section(port));
    follower.stint(&config(), &followed, Duration::from_secs(2)).await.unwrap();
    server.abort();
    let (last, head) = follower.tip().unwrap();
    assert!(last > tip, "the stint took the operator's later commands");
    let status = link.status(1).unwrap();
    let durable = status.durable.expect("a durable head");
    assert_eq!((durable.seq, durable.link), (last, head), "durable through the copy's last command");
    assert_eq!(status.sealer, enclave(), "the key that sealed it");
}

/// A stint syncs its copy once a burst of entries, not once an entry: a sync takes milliseconds, so a
/// copy synced at every entry takes the stream no faster than its disk syncs, and trails the operator.
/// Every entry is in the copy when the stint ends.
#[tokio::test]
async fn a_stint_syncs_its_copy_once_a_burst_not_once_an_entry() {
    let operator = scratch_dir("standby-sync-burst-operator");
    ticks(&operator, 60).await;
    let frames: Vec<_> =
        log_messages(&operator, Offset::start_of(0)).into_iter().map(|message| (By::Operator, message)).collect();
    let taken = frames.len();
    let (port, server, _) = operator_stream(frames).await;
    let dir = scratch_dir("standby-sync-burst-copy");
    let (mut writer, mut copied) = open_copy(&dir, 1024, Chain::new(DOMAIN, enclave())).unwrap();
    let followed = Followed::from_standby(&standby_section(port));
    follow(&mut writer, &mut copied, &config(), &followed, Duration::from_secs(1), None, None).await.unwrap();
    server.abort();
    let syncs = writer.sync_count();
    assert!(syncs * 10 < taken, "{syncs} syncs for the {taken} entries of one burst");
    drop(writer);
    assert_eq!(entries(&dir), log_entries(&operator), "the copy lacks entries the stint took");
}

/// A member's replica keeps its dashboard record's stats worker in step as it follows: each command a
/// stint applies reaches the worker, so a start from the replica takes the worker on with its copy of the
/// state at the replica's last command, and makes no copy of its own.
#[tokio::test]
async fn a_stint_hands_the_stats_worker_each_command_it_applies() {
    let operator = scratch_dir("standby-stats-operator");
    ticks(&operator, 2).await;
    let dir = scratch_dir("standby-stats-copy");
    copy_log(&operator, &dir);
    ticks(&operator, 3).await;
    let stats = crate::test_support::stats();
    let mut follower =
        Follower::open(&dir, 1024, DOMAIN, enclave(), [enclave()].into(), Some(stats.clone()), |_| {}).unwrap();
    let (tip, _) = follower.tip().unwrap();
    let frames = log_messages(&operator, Offset::start_of(tip)).into_iter().map(|message| (By::Operator, message));
    let (port, server, _) = operator_stream(frames.collect()).await;
    let followed = Followed::from_standby(&standby_section(port));
    follower.stint(&config(), &followed, Duration::from_secs(2)).await.unwrap();
    server.abort();
    let (last, _) = follower.tip().unwrap();
    assert!(last > tip, "the stint took the operator's later commands");
    // A copy made at the start would be of this other state.
    stats.scan_in_background(&engine_core::Engine::default());
    assert_eq!(crate::stats::tests::worker_seq(&stats), Some(last), "the worker is not at the replica's last command");
}
