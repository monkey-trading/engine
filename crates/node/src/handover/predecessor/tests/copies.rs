//! A whole-log handover onto a copy restored from an archive mirror, as an election's successor
//! restores it (`sync_predecessor_log`) before it connects: a mirror that spells a seal otherwise
//! hands over the predecessor's history, whose heads the successor's log holds, and one that dropped
//! or replaced a seal is refused before the successor asks for the keys, while the predecessor
//! sequences on.

use super::*;

use std::path::Path;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;

use crate::handover::{Source, sync_predecessor_log};
use crate::log::{MANIFEST, Manifest};
use crate::sequencer::Request as SequencerRequest;
use crate::test_support::{TestSequencer, log_entries, rewrite_command, upper_case_seal};

/// The mirror spelt the seals of the log's first command and of the last command it serves in upper
/// case. The successor restores that copy, takes the rest with the keys, and its log holds the
/// predecessor's cumulative command prefix at every seq, so every head the predecessor posted.
#[tokio::test]
async fn a_whole_log_handover_onto_a_copy_a_mirror_spelt_otherwise_holds_the_predecessors_heads() {
    let operator = scratch_dir("copies-handover-spelt-operator");
    let node = TestSequencer::start(&operator, 1024);
    ticks(&node, 12).await;
    let mut exchange =
        Exchange::start_with("copies-handover-spelt", Some(node.tx.clone()), Some(operator.clone())).await;
    exchange.make_upgrade_current();
    let mirror = mirror_of(&operator, "copies-handover-spelt-mirror", |dir, last| {
        rewrite_command(dir, 0, upper_case_seal);
        rewrite_command(dir, last, upper_case_seal);
    });
    exchange.successor.tee.sealing = Some(mirrored(&exchange, &archive(&mirror).await));

    sync_predecessor_log(&exchange.successor, &incumbent(), (signers(&exchange), None), &exchange.metrics)
        .await
        .unwrap();
    assert_eq!(
        log_entries(&exchange.successor_log),
        log_entries(&mirror),
        "the successor did not restore the mirror's copy"
    );
    let stopped = answer_stop(&mut exchange, node, operator.clone());
    exchange.take().await.unwrap_or_else(|e| panic!("the successor did not take the keys: {e:#}"));
    let head = stopped.await.unwrap();

    let held = crate::sequencer::replay_path(&operator).unwrap().prefix_ring;
    let taken = crate::sequencer::replay_path(&exchange.successor_log).unwrap().prefix_ring;
    for seq in 0..=head {
        assert_eq!(
            taken.prefix_at(seq),
            held.prefix_at(seq),
            "the successor's link at seq {seq} is not the predecessor's"
        );
    }
}

/// The restore reads the bucket the successor's own `[archive]` names before the mirror: the segments the mirror
/// no longer holds, which it answers `404` for, come from the bucket's ranges of the log's chain and vault.
#[tokio::test]
async fn the_archive_restore_reads_the_bucket_the_successors_archive_section_names() {
    let operator = scratch_dir("copies-bucket-operator");
    let node = TestSequencer::start(&operator, 1024);
    ticks(&node, 30).await;
    let mut exchange = Exchange::start_with("copies-bucket", Some(node.tx.clone()), Some(operator.clone())).await;
    let mirror = mirror_of(&operator, "copies-bucket-mirror", |_, _| {});
    let view = crate::test_support::published_manifest(&mirror).borrow().clone();
    let ranges = view.unlisted_from().unwrap() / crate::archive::RANGE_SEQS;
    assert!(ranges > 0, "the log holds a whole range");
    let bucket = crate::archive::stand_in::StandIn::start().await;
    let prefix = crate::archive::log_prefix(
        exchange.successor.custody.chain_id,
        exchange.successor.custody.vault.parse().unwrap(),
    );
    for index in 0..ranges {
        let object = crate::archive::compress(&crate::archive::range_bytes(&mirror, &view, index).unwrap()).unwrap();
        bucket.with(|b| b.objects.insert(crate::archive::range_key(&prefix, index), object));
    }
    let held: Vec<_> =
        view.segments.iter().filter(|listed| listed.last_seq < ranges * crate::archive::RANGE_SEQS).collect();
    assert!(!held.is_empty(), "a listed segment lies in the bucket's ranges");
    for listed in &held {
        std::fs::remove_file(mirror.join(&listed.file)).unwrap();
    }
    let url = archive(&mirror).await;
    let sealing = format!(
        "dir = {:?}\n[handover]\nlisten = '127.0.0.1:0'\nmirrors = [{url:?}]\ndanger_accept_invalid_certs = true",
        exchange.successor_dir.display().to_string()
    );
    exchange.successor.tee.sealing = Some(toml::from_str(&sealing).unwrap());
    exchange.successor.archive = Some(bucket.section(std::time::Duration::from_secs(60)));

    sync_predecessor_log(&exchange.successor, &incumbent(), (signers(&exchange), None), &exchange.metrics)
        .await
        .unwrap();
    assert!(bucket.with(|b| b.read) > 0, "the segments the mirror lacks came from the bucket");
    let restored = Manifest::load(&exchange.successor_log).unwrap();
    assert!(held.iter().all(|listed| restored.segments.iter().any(|have| have.file == listed.file)));
    node.stop().await;
}

/// The archive restore before an election's whole-log take finishes a cut a crash interrupted after the
/// truncation listed the copy's end state, while the later segments are still there unlisted, so the
/// copy does not open by itself: the restore extends the finished cut to the mirror's copy.
#[tokio::test]
async fn the_archive_restore_finishes_a_marked_cut_before_it_extends_the_copy() {
    let operator = scratch_dir("copies-restore-marked-operator");
    let node = TestSequencer::start(&operator, 1024);
    ticks(&node, 12).await;
    let mut exchange =
        Exchange::start_with("copies-restore-marked", Some(node.tx.clone()), Some(operator.clone())).await;
    let mirror = mirror_of(&operator, "copies-restore-marked-mirror", |_, _| {});
    exchange.successor.tee.sealing = Some(mirrored(&exchange, &archive(&mirror).await));
    let log = exchange.successor_log.clone();
    crate::test_support::copy_dir(&mirror, &log);
    // The first checkpoint that ends a listed segment, so the cut trims nothing and the restore extends it.
    let first = Manifest::load(&log).unwrap().segments.iter().find_map(|listed| {
        let text = std::fs::read_to_string(log.join(&listed.file)).unwrap();
        match serde_json::from_str::<crate::log::LogEntry>(text.lines().last()?).ok()? {
            crate::log::LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
            _ => None,
        }
    });
    let first = first.expect("a listed segment of the copy ends at a checkpoint");
    // The state a crash after the truncation's manifest leaves: that manifest, every file, the marker.
    let cut = scratch_dir("copies-restore-marked-cut");
    crate::test_support::copy_dir(&log, &cut);
    crate::log::truncate_to_checkpoint(&cut, first.seq, &first.root).unwrap();
    std::fs::copy(cut.join(MANIFEST), log.join(MANIFEST)).unwrap();
    let signer = AccountId(crate::tee::EnclaveKey::test_key().address());
    let marker = crate::checkpoints::Takeover { seq: first.seq, root: first.root.clone(), signer };
    std::fs::write(log.join("takeover.json"), serde_json::to_vec(&marker).unwrap()).unwrap();
    assert!(crate::log::SegmentedLog::open(&log, |_| Ok(())).is_err(), "the interrupted cut opens by itself");

    sync_predecessor_log(&exchange.successor, &incumbent(), (signers(&exchange), None), &exchange.metrics)
        .await
        .unwrap_or_else(|e| panic!("the restore refused the copy: {e:#}"));
    assert!(!log.join("takeover.json").exists(), "the marker outlived the restore");
    assert_eq!(log_entries(&log), log_entries(&mirror), "the restore did not extend the finished cut");
    node.stop().await;
}

/// A whole-log handover onto a copy holding the marker of a cut a crash interrupted after it completed
/// finishes the cut before it reads the copy's tip, so the marker is gone once the keys are taken and
/// cannot cut the handed-over tail afterwards; the successor's log holds the predecessor's prefix.
#[tokio::test]
async fn a_whole_log_handover_finishes_a_marked_cut_before_it_takes_the_log() {
    let operator = scratch_dir("copies-handover-marked-operator");
    let node = TestSequencer::start(&operator, 1024);
    ticks(&node, 12).await;
    let mut exchange =
        Exchange::start_with("copies-handover-marked", Some(node.tx.clone()), Some(operator.clone())).await;
    exchange.make_upgrade_current();
    let mirror = mirror_of(&operator, "copies-handover-marked-mirror", |_, _| {});
    exchange.successor.tee.sealing = Some(mirrored(&exchange, &archive(&mirror).await));
    sync_predecessor_log(&exchange.successor, &incumbent(), (signers(&exchange), None), &exchange.metrics)
        .await
        .unwrap();
    // The last checkpoint that ends a listed segment of the restored copy: a cut to it trims nothing.
    let log = &exchange.successor_log;
    let ended = Manifest::load(log).unwrap().segments.iter().rev().find_map(|listed| {
        let text = std::fs::read_to_string(log.join(&listed.file)).unwrap();
        match serde_json::from_str::<crate::log::LogEntry>(text.lines().last()?).ok()? {
            crate::log::LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
            _ => None,
        }
    });
    let ended = ended.expect("a listed segment of the copy ends at a checkpoint");
    let signer = AccountId(crate::tee::EnclaveKey::test_key().address());
    let marker = crate::checkpoints::Takeover { seq: ended.seq, root: ended.root.clone(), signer };
    std::fs::write(log.join("takeover.json"), serde_json::to_vec(&marker).unwrap()).unwrap();

    let stopped = answer_stop(&mut exchange, node, operator.clone());
    exchange.take().await.unwrap_or_else(|e| panic!("the successor did not take the keys: {e:#}"));
    let head = stopped.await.unwrap();
    assert!(!exchange.successor_log.join("takeover.json").exists(), "the marker outlived the take");
    let held = crate::sequencer::replay_path(&operator).unwrap().prefix_ring;
    let taken = crate::sequencer::replay_path(&exchange.successor_log).unwrap().prefix_ring;
    assert_eq!(taken.prefix_at(head), held.prefix_at(head), "the successor's log lacks the predecessor's head");
}

/// A mirror that dropped a seal, or replaced it with another key's, which a later checkpoint covers:
/// the restore (`sync_predecessor_log`), which an election's and a turn's whole-log take run before they
/// ask for the keys, refuses the copy, and the predecessor sequences on.
#[tokio::test]
async fn a_restore_refuses_a_copy_a_mirror_dropped_or_replaced_a_seal_in() {
    let other = crate::tee::EnclaveKey::generate();
    for how in ["dropped", "replaced"] {
        let name = format!("copies-handover-{how}");
        let operator = scratch_dir(&format!("{name}-operator"));
        let node = TestSequencer::start(&operator, 1024);
        ticks(&node, 12).await;
        let mut exchange = Exchange::start_with(&name, Some(node.tx.clone()), Some(operator.clone())).await;
        exchange.make_upgrade_current();
        let mirror = mirror_of(&operator, &format!("{name}-mirror"), |dir, _| {
            rewrite_command(dir, 0, |command| {
                command.seal = match how {
                    "dropped" => None,
                    _ => Some(other.seal_command(
                        exchange.domain(),
                        crate::log::command_seal::command_link(Default::default(), command).unwrap(),
                    )),
                }
            })
        });
        exchange.successor.tee.sealing = Some(mirrored(&exchange, &archive(&mirror).await));

        let refused =
            sync_predecessor_log(&exchange.successor, &incumbent(), (signers(&exchange), None), &exchange.metrics)
                .await
                .err()
                .unwrap_or_else(|| panic!("{how}: the successor restored a copy without its writer's seals"));
        assert!(format!("{refused:#}").contains("seq 0 carries no seal by an accepted signer"), "{how}: {refused:#}");
        assert!(exchange.drains.try_recv().is_err(), "{how}: the predecessor was asked to stop");
        assert!(!exchange.run.is_finished(), "{how}: the predecessor stopped serving");
        ticks(&node, 1).await;
        node.stop().await;
    }
}

/// A successor whose restored copy dropped a seal, or holds another key's, which a checkpoint covers:
/// the replay `take_segments` runs before the confirmation refuses it, so nothing is sealed and the
/// predecessor sees no confirmation.
#[tokio::test]
async fn a_successor_whose_restored_copy_dropped_or_replaced_a_seal_confirms_nothing() {
    let other = crate::tee::EnclaveKey::generate();
    for how in ["dropped", "replaced"] {
        let mut exchange = Exchange::start(&format!("copies-take-{how}")).await;
        exchange.make_upgrade_current();
        let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
        let domain = exchange.domain();
        rewrite_command(&exchange.successor_log, 0, |command| {
            command.seal = match how {
                "dropped" => None,
                _ => Some(other.seal_command(
                    domain,
                    crate::log::command_seal::command_link(Default::default(), command).unwrap(),
                )),
            }
        });
        let _answer = exchange.answer_drain(Ok(head));
        let refused = exchange.take().await.err().unwrap_or_else(|| panic!("{how}: the successor confirmed"));
        assert!(format!("{refused:#}").contains("seq 0 carries no seal by an accepted signer"), "{how}: {refused:#}");
        assert!(!file_for(&exchange.successor_dir, &exchange.upgrade(), GENERATION).exists(), "{how}: keys sealed");
        assert!(!exchange.run.is_finished(), "{how}: the predecessor ended without a confirmation");
    }
}

// ── Helpers ──

/// Sequences `count` ticks on `node`.
pub(super) async fn ticks(node: &TestSequencer, count: usize) {
    for _ in 0..count {
        node.submit(Command::Tick).await;
    }
}

/// What an archive mirror of the log in `operator` serves, in a new directory named for `name`: its
/// completed segments and their listings, changed by `alter`, which is given the last command they
/// hold.
pub(super) fn mirror_of(operator: &Path, name: &str, alter: impl FnOnce(&Path, u64)) -> PathBuf {
    let mirror = scratch_dir(name);
    std::fs::create_dir_all(&mirror).unwrap();
    let listings = Manifest::load(operator).unwrap().segments;
    assert!(listings.len() >= 2, "the archive lists at least two completed segments");
    for listed in listings.iter() {
        std::fs::copy(operator.join(&listed.file), mirror.join(&listed.file)).unwrap();
    }
    std::fs::copy(operator.join(MANIFEST), mirror.join(MANIFEST)).unwrap();
    alter(&mirror, listings.last().unwrap().last_seq);
    mirror
}

/// The successor's sealing section, with `mirror` as its only archive mirror.
pub(super) fn mirrored(exchange: &Exchange, mirror: &str) -> crate::config::SealingSection {
    let sealing = format!(
        "dir = {:?}\n[handover]\nlisten = '127.0.0.1:0'\nmirrors = [{mirror:?}]",
        exchange.successor_dir.display().to_string()
    );
    toml::from_str(&sealing).unwrap()
}

/// The signers the vault recorded, as the successor reads them before restoring the log.
pub(super) fn signers(exchange: &Exchange) -> std::collections::BTreeSet<AccountId> {
    let old = AccountId(exchange.old.enclave_key().unwrap().address());
    [old, AccountId(crate::tee::EnclaveKey::test_key().address())].into()
}

/// The incumbent, whose own API hostname nothing answers on: the mirror alone serves its archive.
pub(super) fn incumbent() -> Source {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    Source { endpoint: closed.clone(), api_hostname: closed, release: [0x22; 32], candidate: AccountId([0x42; 20]) }
}

/// Answers the predecessor's stop as the node does: a final checkpoint through the sequencer, which
/// seals the log's tail segment, then the drained head. The task ends with the head's seq.
pub(super) fn answer_stop(exchange: &mut Exchange, node: TestSequencer, operator: PathBuf) -> JoinHandle<u64> {
    let mut drains = std::mem::replace(&mut exchange.drains, mpsc::channel(1).1);
    tokio::spawn(async move {
        let reply = drains.recv().await.expect("the predecessor asks to stop");
        let (stop, stopped) = oneshot::channel();
        node.tx.send(SequencerRequest::FinalCheckpoint(stop)).await.unwrap();
        stopped.await.unwrap().unwrap();
        node.stop().await;
        let Some(LogEntry::Checkpoint(last)) = log_entries(&operator).pop() else {
            panic!("the drained log ends at its final checkpoint")
        };
        let _ = reply.send(Ok(Head { seq: last.seq, root: last.root }));
        last.seq
    })
}

/// Serves the files in `dir` as an archive mirror, over plain HTTP on loopback: its base URL.
pub(super) async fn archive(dir: &Path) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let dir = dir.to_owned();
    let app = Router::new().route(
        "/v1/archive/{file}",
        get(move |axum::extract::Path(file): axum::extract::Path<String>| {
            let path = dir.join(&file);
            async move {
                match std::fs::read(path) {
                    Ok(bytes) => bytes.into_response(),
                    Err(_) => StatusCode::NOT_FOUND.into_response(),
                }
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}
