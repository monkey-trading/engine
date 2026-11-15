//! A member's copy records as durable only what it appended, with the key that sealed it.

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
    assert_eq!((status.durable.seq, status.durable.link), (last, head), "durable through the copy's last command");
    assert_eq!(status.sealer, enclave(), "the key that sealed it");
}
