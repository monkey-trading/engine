//! Batching: a batch ends after `MAX_BATCH` requests of any kind.

use super::*;
use tokio::sync::oneshot;

use crate::log::Manifest;
use crate::test_support::{log_entries, scratch_dir, sequencer_config, trusted_now};

/// Reply-less commands keep the channel full, and a client's reply still goes out with the first
/// `MAX_BATCH` requests, while the rest wait for later batches.
#[tokio::test]
async fn a_reply_goes_out_at_the_batch_limit_while_reply_less_commands_keep_coming() {
    let seq = sequencer(&scratch_dir("seq-batch-flood"));
    let first = seq.next_seq;
    let (tx, rx) = mpsc::channel(3 * MAX_BATCH);
    let (reply, replied) = oneshot::channel();
    tx.try_send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).unwrap();
    let (at_end, seen) = oneshot::channel();
    tx.try_send(Request::Query(Box::new(move |e: &Engine| {
        let _ = at_end.send(e.last_seq());
    })))
    .unwrap();
    for _ in 0..2 * MAX_BATCH {
        tx.try_send(Request::Submit { cmd: Command::Tick, reply: None }).unwrap();
    }
    let task = tokio::spawn(seq.run(rx));
    replied.await.unwrap();
    // The query, in the reply's batch, sees it end after `MAX_BATCH` requests.
    assert_eq!(seen.await.unwrap(), Some(first + MAX_BATCH as u64 - 2));
    drop(tx);
    task.await.unwrap().unwrap();
}

/// A batch ends after `MAX_BATCH` requests whatever their kind: a query that is a batch's last
/// request runs before the command after it is applied.
#[tokio::test]
async fn a_batch_ends_after_max_batch_requests_of_any_kind() {
    for kind in ["submit without reply", "submit with reply", "time sample", "sync", "query"] {
        let dir = scratch_dir(&format!("seq-batch-{}", kind.replace(' ', "-")));
        let seq = sequencer(&dir);
        let (tx, rx) = mpsc::channel(MAX_BATCH + 1);
        for _ in 0..MAX_BATCH - 1 {
            let request = match kind {
                "submit without reply" => Request::Submit { cmd: Command::Tick, reply: None },
                "submit with reply" => Request::Submit { cmd: Command::Tick, reply: Some(oneshot::channel().0) },
                "time sample" => Request::TimeSample(trusted_now()),
                "sync" => Request::Sync(oneshot::channel().0),
                _ => Request::Query(Box::new(|_: &Engine| {})),
            };
            tx.try_send(request).unwrap();
        }
        let (at_end, seen) = oneshot::channel();
        tx.try_send(Request::Query(Box::new(move |e: &Engine| {
            let _ = at_end.send(e.last_seq());
        })))
        .unwrap();
        tx.try_send(Request::Submit { cmd: Command::Tick, reply: None }).unwrap();
        drop(tx);
        seq.run(rx).await.unwrap();
        let after = seen.await.unwrap().map_or(0, |s| s + 1);
        assert_eq!(replay(&log_entries(&dir)).unwrap().engine.last_seq(), Some(after), "{kind}");
    }
}

// ── Helpers ──

/// A sequencer on a fresh log in `dir`, with trusted time admitting stamps on the system clock.
fn sequencer(dir: &Path) -> Sequencer {
    let (stream, _) = broadcast::channel(16);
    let (withdrawals, _) = mpsc::unbounded_channel();
    let (manifest, _) = watch::channel(Manifest::default());
    let key = Arc::new(EnclaveKey::generate());
    let mut seq = Sequencer::recover(sequencer_config(dir, manifest), key, stream, withdrawals).unwrap();
    seq.trust(trusted_now()).unwrap();
    seq
}
