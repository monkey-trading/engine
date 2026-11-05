//! The rollback check over the replayed bytes: seqs the log lacks, a link mismatch at the
//! posted seq, a seq the replay did not capture, and the matching posted head.

use alloy::primitives::B256;

use super::Head;
use super::check_rollback;

#[test]
fn the_rollback_check_refuses_a_posted_head_the_log_lacks() {
    let posted = Head { seq: 10, link: B256::from([1; 32]) };
    let error = check_rollback(posted, 8, |_| panic!("the log's link is not consulted")).unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("9..=10"), "{error}");
}

#[test]
fn the_rollback_check_refuses_a_link_mismatch_at_the_posted_seq() {
    let posted = Head { seq: 10, link: B256::from([1; 32]) };
    let error = check_rollback(posted, 12, |seq| {
        assert_eq!(seq, 10);
        Ok(Some(B256::from([2; 32])))
    })
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("differs"), "{error}");
}

#[test]
fn the_rollback_check_refuses_a_seq_the_replay_did_not_capture() {
    let posted = Head { seq: 10, link: B256::from([1; 32]) };
    let error = check_rollback(posted, 12, |seq| {
        assert_eq!(seq, 10);
        anyhow::bail!("the replayed log holds no command at the posted head's seq 10")
    })
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("holds no command"), "{error}");
}

#[test]
fn the_rollback_check_accepts_a_seq_only_attested_head() {
    // At or under an attested snapshot base the replay never reaches the seq; the snapshot
    // determines the content, and so the link: the seq check is the binding one.
    let posted = Head { seq: 10, link: B256::from([1; 32]) };
    check_rollback(posted, 12, |seq| {
        assert_eq!(seq, 10);
        Ok(None)
    })
    .unwrap();
}

#[test]
fn the_rollback_check_accepts_the_matching_posted_head() {
    let posted = Head { seq: 10, link: B256::from([1; 32]) };
    check_rollback(posted, 12, |seq| {
        assert_eq!(seq, 10);
        Ok(Some(posted.link))
    })
    .unwrap();
}
