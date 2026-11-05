//! The balance read that fails: nothing changes until it has failed for a while, and then it warns.

use super::*;
use crate::seed_relay::balance::UNREADABLE_AFTER;

#[tokio::test(start_paused = true)]
async fn a_balance_unreadable_for_a_bounded_time_warns_naming_the_cause_until_it_reads_again() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let fake = Fake::new(slot_past(FINAL + 2));
    *fake.balance_error.lock().unwrap() = Some("the providers disagree on their finalized block".to_owned());
    let mut relayer = relayer(&fake);
    relayer.poll().await;
    assert!(named(&mut alerts, "seed_relayer_balance_unreadable").is_empty(), "one failed read is no episode");
    tokio::time::advance(UNREADABLE_AFTER).await;
    relayer.poll().await;
    let warned = named(&mut alerts, "seed_relayer_balance_unreadable");
    assert_eq!(warned.len(), 1);
    assert_eq!(warned[0]["level"], "WARN");
    assert!(warned[0]["message"].contains("the providers disagree on their finalized block"), "{warned:?}");
    // The episode alerts once, and ends with the first read.
    relayer.poll().await;
    assert!(named(&mut alerts, "seed_relayer_balance_unreadable").is_empty());
    *fake.balance_error.lock().unwrap() = None;
    relayer.poll().await;
    assert_eq!(named(&mut resolved, "seed_relayer_balance_unreadable").len(), 1);
}
