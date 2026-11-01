//! The handover's stop and the final checkpoint it hands over: refused with the checkpoint gate's
//! causes while it would withhold the checkpoint, ended only when the drained log has the final
//! checkpoint.

use super::*;

#[tokio::test]
async fn a_stop_the_checkpoint_gate_refuses_keeps_the_keys_and_goes_on_sequencing() {
    let mut exchange = Exchange::start("handover-gate-refused").await;
    exchange.make_upgrade_current();
    let answer = exchange.refuse_once_then_answer(
        "the final checkpoint is withheld (collateral): the handover is refused and this node goes on \
         sequencing until the checkpoint_withheld alert clears",
        Head { seq: 3, root: format!("0x{}", "cd".repeat(32)) },
    );
    let err = exchange.take().await.err().expect("keys left while the gate withholds the final checkpoint");
    let err = format!("{err:#}");
    assert!(err.contains("withheld"), "{err}");
    assert!(
        !file_for(&exchange.successor_dir, &exchange.upgrade(), GENERATION).exists(),
        "the refused stop sealed nothing"
    );
    assert!(!exchange.run.is_finished(), "the refused stop ended the predecessor");

    // Once the gate admits the final checkpoint, the next stop takes the keys and ends it.
    write_log_tail(&exchange.successor_log, &Head { seq: 3, root: format!("0x{}", "cd".repeat(32)) });
    exchange.take().await.unwrap();
    timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
    assert_eq!(answer.await.unwrap(), 2, "the refused stop and the recovered one");
}

#[tokio::test]
async fn a_final_checkpoint_missing_after_the_committed_stop_ends_the_predecessor() {
    let (_guard, mut alerts) = capture_alerts();
    let mut exchange = Exchange::start("handover-withheld-after-commit").await;
    exchange.make_upgrade_current();
    // The stop committed and the drained log then had no final checkpoint: a defense the stop's
    // request makes unreachable in normal operation, kept for the crash windows it still covers.
    let _answer = exchange.answer_drain(Err(anyhow!(
        "the final checkpoint was withheld after seq 3, so the keys stay with this release: the vault \
         already names the successor and a restart of this release refuses, so the successor needs new \
         keys, which the timelock registers"
    )));
    let err = exchange.take().await.err().expect("keys left without a final checkpoint");
    let err = format!("{err:#}");
    assert!(err.contains("withheld"), "{err}");
    assert!(!file_for(&exchange.successor_dir, &exchange.upgrade(), GENERATION).exists());
    let alert = timeout(Duration::from_secs(5), alerts.recv()).await.expect("no alert").unwrap();
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("handover_failed", "ERROR"));
    let ended = timeout(Duration::from_secs(5), exchange.run).await.expect("the predecessor ended").unwrap();
    assert!(ended.is_err(), "a committed stop with nothing to hand over kept serving");
}

#[tokio::test]
async fn a_predecessor_whose_checkpoints_are_withheld_goes_on_sequencing() {
    let mut exchange = Exchange::start("handover-signing-withheld").await;
    exchange.make_upgrade_current();
    exchange.signing.send_replace(false);
    let err = exchange.take().await.err().expect("a predecessor that cannot sign a final checkpoint stopped");
    assert!(format!("{err:#}").contains("checkpoint signing is withheld"), "{err:#}");
    assert!(exchange.drains.try_recv().is_err(), "sequencing stopped while checkpoints are withheld");
    assert!(!exchange.run.is_finished(), "the predecessor stopped serving");

    // Once signing resumes, the next successor takes the keys.
    exchange.signing.send_replace(true);
    let head = Head { seq: 3, root: format!("0x{}", "cd".repeat(32)) };
    write_log_tail(&exchange.successor_log, &head);
    let _answer = exchange.answer_drain(Ok(head));
    exchange.take().await.unwrap();
}

#[test]
fn the_final_head_is_the_checkpoint_of_the_last_command_the_sequencer_published() {
    let root = |byte: &str| format!("0x{}", byte.repeat(32));
    let recovered = checkpoint(5, &root("aa"));

    // Nothing published since recovery: the recovered log's last checkpoint.
    let (tx, rx) = broadcast::channel(4);
    let head = Published::new(rx, Some(5), Some(&recovered)).final_head().unwrap();
    assert_eq!(head, Head { seq: 5, root: root("aa") });

    // A command and then its checkpoint.
    let rx = tx.subscribe();
    publish(&tx, LogEntry::Command(command(6)));
    publish(&tx, LogEntry::Checkpoint(checkpoint(6, &root("bb"))));
    assert_eq!(Published::new(rx, Some(5), Some(&recovered)).final_head().unwrap().seq, 6);

    // A last command the drain did not checkpoint, and a log with no checkpoint at all.
    let rx = tx.subscribe();
    publish(&tx, LogEntry::Command(command(7)));
    let err = Published::new(rx, Some(6), Some(&checkpoint(6, &root("bb")))).final_head().unwrap_err();
    assert!(err.to_string().contains("withheld"), "{err:#}");
    assert!(
        err.to_string().contains("timelock"),
        "the error names the real recovery, new keys through the timelock: {err:#}"
    );
    let (_tx, rx) = broadcast::channel::<Arc<StreamMsg>>(4);
    assert!(Published::new(rx, Some(3), None).final_head().is_err(), "a log with no checkpoint handed over");

    // A receiver that fell behind still sees the newest entries.
    let (tx, rx) = broadcast::channel(2);
    (6..40).for_each(|seq| publish(&tx, LogEntry::Command(command(seq))));
    publish(&tx, LogEntry::Checkpoint(checkpoint(39, &root("cc"))));
    assert_eq!(Published::new(rx, Some(5), Some(&recovered)).final_head().unwrap().seq, 39);
}
