use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use engine_core::types::Side;

use super::*;
use crate::stats::background::{Background, Job, Point, ReadJob, Reader};

fn json(value: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

/// The commands both records see: trades on both sides of a scan falling due, then more after it, so the
/// state has moved on by the time the background scan is installed.
fn script(rig: &mut Rig) {
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 2);
    rig.trade(T0 + 2_000, OTHER, Side::Sell, 1);
    rig.price(T0 + 30_000, 101_000);
    rig.price(T0 + 60_000, 102_000);
    rig.trade(T0 + 61_000, TRADER, Side::Sell, 2);
    rig.price(T0 + 62_000, 90_000);
}

/// The scan a worker takes is the one an inline record takes at the command it fell due at: the same
/// figures at the same seq and time, the same series, though the engine moved on before it landed.
#[test]
fn a_background_scan_gives_what_an_inline_scan_gives_at_the_command_it_fell_due_at() {
    let mut inline = Rig::new();
    script(&mut inline);

    let mut background = Rig::new();
    background.stats.scan_in_background(&background.engine);
    script(&mut background);
    background.stats.settle();

    let (served_inline, served) =
        (json(inline.stats.house(&inline.engine)), json(background.stats.house(&background.engine)));
    assert_eq!(served["seq"], served_inline["seq"]);
    assert_eq!(served["snapshot_ms"], T0 + 60_000);
    assert!(served["open_interest"].as_str().unwrap().parse::<Fixed>().unwrap().is_positive(), "{served}");
    assert_eq!(served["open_interest"], served_inline["open_interest"]);
    assert_eq!(background.stats.dump(), inline.stats.dump());
    let positions = |rig: &Rig| json(rig.stats.positions(&rig.engine, BTC).unwrap());
    assert_eq!(positions(&background), positions(&inline));
}

/// Every due scan is taken, whatever the worker's speed, so the series are the ones replay gives; the last
/// one is served.
#[test]
fn every_due_scan_is_taken_and_the_last_is_served() {
    let mut rig = Rig::new();
    rig.stats.scan_in_background(&rig.engine);
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 1);
    rig.price(T0 + 60_000, 101_000);
    rig.price(T0 + 120_000, 102_000);
    rig.stats.settle();
    assert_eq!(json(rig.stats.house(&rig.engine))["snapshot_ms"], T0 + 120_000);
    let query = SeriesQuery {
        metric: "long".into(),
        market: Some(BTC),
        resolution: "1m".into(),
        from_ms: None,
        to_ms: None,
        limit: None,
    };
    let series = json(rig.stats.series(&rig.engine, &query).unwrap().finish());
    assert_eq!(series["entries"].as_array().unwrap().len(), 2, "{series}");
}

/// A worker whose copy cannot follow the sequencer says so, and never serves a scan of a state the
/// sequencer was not in.
#[test]
fn a_worker_given_a_command_out_of_sequence_stops_without_scanning() {
    let rig = Rig::new();
    let mut worker = Background::start(&rig.engine).unwrap();
    let skipped = Sequenced { seq: rig.engine.last_seq().unwrap() + 5, ts: T0 + 1_000, cmd: Command::Tick };
    assert!(worker.reader().send(Job::Apply(Box::new(skipped))));
    let stopped = worker.wait().expect_err("the worker refuses the gap");
    assert!(stopped.contains("applying seq"), "{stopped}");
}

/// The worker's copy never computes a root, so nothing but the worker drains its record of changed keys:
/// however many commands it applies the record stays empty, while the sequencer's own engine, which does
/// compute roots, keeps recording what it changes.
#[test]
fn the_workers_copy_does_not_accumulate_changed_keys() {
    let mut rig = Rig::new();
    let worker = Background::start(&rig.engine).unwrap();
    for step in 1..=200_i64 {
        let ts = T0 + step * 1_000;
        let seq = rig.engine.last_seq().unwrap() + 1;
        let cmd = if step % 2 == 0 {
            let quotes = ["a", "b"]
                .map(|source| engine_core::price::SourceQuote {
                    source: source.into(),
                    mid: Fixed::from_int(100_000 + step),
                    ts,
                    contract: None,
                })
                .to_vec();
            Command::Prices { market: BTC, quotes }
        } else {
            Command::Tick
        };
        let sequenced = Sequenced { seq, ts, cmd };
        rig.engine.apply(&sequenced).unwrap();
        assert!(worker.reader().send(Job::Apply(Box::new(sequenced))));
    }
    assert!(rig.engine.changed_key_count() > 0, "the sequencer's engine records until it computes a root");
    let (sent, received) = std::sync::mpsc::channel();
    let read = Box::new(move |engine: &Engine| sent.send(engine.changed_key_count()).unwrap());
    assert!(worker.reader().read(read).is_ok());
    assert_eq!(received.recv().unwrap(), 0);
}

/// A recovery cache's engine, encoded by the worker's copy at the command a checkpoint covers, is the
/// encoding of the sequencer's engine there, byte for byte, though the sequencer moved on before the
/// worker took the job.
#[test]
fn the_workers_copy_encodes_the_engine_the_sequencer_held_at_the_checkpoint() {
    let mut rig = Rig::new();
    rig.stats.scan_in_background(&rig.engine);
    script(&mut rig);
    let (seq, ts) = (rig.engine.last_seq().unwrap(), rig.engine.now());
    let expected = serde_json::to_string(&rig.engine).unwrap();
    let (job, encoded) = rig.stats.encode_job(seq, ts).expect("a worker runs");
    rig.stats.stage_job(job);
    // Commands after the checkpoint reach the worker behind its job.
    rig.trade(T0 + 63_000, OTHER, Side::Buy, 1);
    rig.stats.deliver(rig.stats.stage());
    assert_eq!(encoded.recv().unwrap(), Ok(expected));
}

/// An encode job the worker's copy is not at, by seq or by time, refuses to encode another state, and
/// the worker stops as it does for a scan out of step; with no worker, no job is staged.
#[test]
fn a_worker_asked_to_encode_a_state_it_is_not_in_stops_without_encoding() {
    for (seq_after, ts_after) in [(1, 0), (0, 1)] {
        let rig = Rig::new();
        let mut worker = Background::start(&rig.engine).unwrap();
        let (seq, ts) = (rig.engine.last_seq().unwrap() + seq_after, rig.engine.now() + ts_after);
        let (reply, encoded) = std::sync::mpsc::sync_channel(1);
        assert!(worker.reader().send(Job::Encode { seq, ts, reply }));
        let refused = encoded.recv().unwrap().expect_err("no encoding of another state");
        assert!(refused.contains("the copy is at"), "{refused}");
        assert!(worker.wait().expect_err("the worker stops").contains("the copy is at"));
    }
    let rig = Rig::new();
    assert!(rig.stats.encode_job(rig.engine.last_seq().unwrap(), rig.engine.now()).is_none(), "no worker runs");
}

/// While an encode is queued or runs on the worker, a read is handed back for the sequencer to run, so
/// no read, and no request admitted on one, waits behind it; once the encode is done, reads go to the
/// worker again.
#[test]
fn a_read_never_waits_behind_an_encode() {
    let rig = Rig::new();
    let worker = Background::start(&rig.engine).unwrap();
    let reader = worker.reader();
    // The worker is held in a read, so the encode stays queued behind it.
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (entered, started) = std::sync::mpsc::channel();
    let holding: crate::stats::background::ReadJob = Box::new(move |_| {
        entered.send(()).unwrap();
        held.recv().unwrap();
    });
    assert!(reader.read(holding).is_ok());
    started.recv().unwrap();
    let (reply, encoded) = std::sync::mpsc::sync_channel(1);
    let (seq, ts) = (rig.engine.last_seq().unwrap(), rig.engine.now());
    assert!(reader.send(Job::Encode { seq, ts, reply }));
    assert!(reader.read(Box::new(|_| {})).is_err(), "a read is handed back while the encode is queued");
    release.send(()).unwrap();
    assert_eq!(encoded.recv().unwrap(), Ok(serde_json::to_string(&rig.engine).unwrap()));
    let (sent, received) = std::sync::mpsc::channel();
    let read: crate::stats::background::ReadJob = Box::new(move |engine| sent.send(engine.last_seq()).unwrap());
    assert!(reader.read(read).is_ok(), "reads go to the worker once the encode is done");
    assert_eq!(received.recv().unwrap(), Some(seq));
}

/// The admission lock is what keeps a read from queueing behind an encode counted between its check of the
/// pending encodes and its enqueue: a read holds it from its check to its queueing, and an encode is
/// counted under it.
#[test]
fn admission_is_held_from_a_reads_check_to_its_queueing_and_while_an_encode_is_counted() {
    let rig = Rig::new();
    let worker = Background::start(&rig.engine).unwrap();
    let reader = worker.reader();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    *reader.probe.lock().unwrap() = Some(Arc::new(move |point, held| log.lock().unwrap().push((point, held))));
    assert!(reader.read(Box::new(|_| {})).is_ok());
    let (reply, encoded) = std::sync::mpsc::sync_channel(1);
    let (seq, ts) = (rig.engine.last_seq().unwrap(), rig.engine.now());
    assert!(reader.send(Job::Encode { seq, ts, reply }));
    encoded.recv().unwrap().unwrap();
    assert_eq!(*seen.lock().unwrap(), [(Point::Checked, true), (Point::Counted, true)]);
}

/// Holds the first thread of `reader` to reach `wanted` there until the returned sender is used, and says
/// when it has.
fn hold_at(reader: &Reader, wanted: Point) -> (Receiver<()>, Sender<()>) {
    let (reached, held) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = Mutex::new(released);
    *reader.probe.lock().unwrap() = Some(Arc::new(move |point, _| {
        if point == wanted {
            reached.send(()).unwrap();
            released.lock().unwrap().recv().unwrap();
        }
    }));
    (held, release)
}

/// A read held between its check of the pending encodes and its enqueue queues ahead of an encode
/// delivered meanwhile, which waits for the read to queue.
#[test]
fn an_encode_delivered_while_a_read_is_between_its_check_and_its_queueing_queues_behind_it() {
    let rig = Rig::new();
    let worker = Background::start(&rig.engine).unwrap();
    let reader = worker.reader();
    let (seq, ts) = (rig.engine.last_seq().unwrap(), rig.engine.now());
    let (held, release) = hold_at(&reader, Point::Checked);
    let (reply, encoded) = std::sync::mpsc::sync_channel(1);
    // Whether the encode's reply was sent by the time the read ran on the worker.
    let (ran, behind) = std::sync::mpsc::channel();
    let read: ReadJob = Box::new(move |_| ran.send(encoded.try_recv().is_ok()).unwrap());
    let reading = std::thread::spawn({
        let reader = reader.clone();
        move || reader.read(read).is_ok()
    });
    held.recv().unwrap();
    let sending = std::thread::spawn({
        let reader = reader.clone();
        move || reader.send(Job::Encode { seq, ts, reply })
    });
    release.send(()).unwrap();
    assert!(reading.join().unwrap(), "the read found no encode pending and queued");
    assert!(sending.join().unwrap());
    assert!(!behind.recv().unwrap(), "the read ran behind the encode");
}

/// A read arriving once an encode is counted and before it queues is handed back, for the sequencer to
/// run, whatever the worker has done since.
#[test]
fn a_read_arriving_while_an_encode_is_counted_and_not_queued_is_handed_back() {
    let rig = Rig::new();
    let worker = Background::start(&rig.engine).unwrap();
    let reader = worker.reader();
    let (seq, ts) = (rig.engine.last_seq().unwrap(), rig.engine.now());
    // The worker stays in a read, so the encode cannot finish and uncount itself.
    let (free, worker_held) = std::sync::mpsc::channel::<()>();
    let (entered, started) = std::sync::mpsc::channel();
    assert!(
        reader
            .read(Box::new(move |_| {
                entered.send(()).unwrap();
                worker_held.recv().unwrap();
            }))
            .is_ok()
    );
    started.recv().unwrap();
    let (held, release) = hold_at(&reader, Point::Counted);
    let (reply, encoded) = std::sync::mpsc::sync_channel(1);
    let sending = std::thread::spawn({
        let reader = reader.clone();
        move || reader.send(Job::Encode { seq, ts, reply })
    });
    held.recv().unwrap();
    let reading = std::thread::spawn({
        let reader = reader.clone();
        move || reader.read(Box::new(|_| {})).is_err()
    });
    release.send(()).unwrap();
    assert!(sending.join().unwrap());
    assert!(reading.join().unwrap(), "the read was handed back");
    free.send(()).unwrap();
    encoded.recv().unwrap().unwrap();
}

/// A start whose record already runs a worker, as a member's replica keeps one in step as it follows,
/// keeps it, making no copy of the state, and takes the recovery's full scan from it, delivered with the
/// first durable batch, rather than scanning every account on the starting thread; the scan is served
/// and left out of the series, as an inline recovery's is.
#[test]
fn a_start_keeps_a_running_worker_and_takes_its_full_scan_from_it() {
    let replayed = stats();
    replayed.restart();
    let mut rig = Rig::recording_into(replayed.clone());
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 2);
    // The replica's worker starts after its build, and takes each command it follows.
    replayed.scan_in_background(&rig.engine);
    rig.trade(T0 + 2_000, OTHER, Side::Sell, 1);
    replayed.deliver(replayed.stage());

    replayed.finish_recovery(&rig.engine);
    assert!(replayed.positions(&rig.engine, BTC).unwrap().is_none(), "the start scanned on its own thread");
    // A copy made at the start would be of this other state.
    replayed.scan_in_background(&Engine::default());
    assert_eq!(super::worker_seq(&replayed), rig.engine.last_seq(), "the start replaced the running worker");
    replayed.settle();
    let house = json(replayed.house(&rig.engine));
    assert_eq!(house["seq"], rig.engine.last_seq().unwrap(), "{house}");
    assert!(house["open_margin"].as_str().unwrap().parse::<Fixed>().unwrap().is_positive(), "{house}");
    assert!(replayed.positions(&rig.engine, BTC).unwrap().is_some(), "the worker's scan is full");
    // Served, not sampled: the series are those an inline recovery of the same commands leaves.
    let inline = stats();
    inline.restart();
    let mut same = Rig::recording_into(inline.clone());
    same.trade(T0 + 1_000, TRADER, Side::Buy, 2);
    same.trade(T0 + 2_000, OTHER, Side::Sell, 1);
    inline.finish_recovery(&same.engine);
    assert_eq!(replayed.dump(), inline.dump(), "the start's scan went into the series");
}
