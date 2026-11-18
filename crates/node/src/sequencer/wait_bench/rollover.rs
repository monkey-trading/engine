//! What a price waits behind the daily rollover and the MONKEY period close on the node's sequencer
//! (#933): a weekday rollover, which finances every position on the core's launch book, and the close
//! of an emission period every account lost in, each through the real `run` loop with prices 2 ms
//! apart, as `mass` runs #907's close-outs. Each prints the time the command held the thread and the
//! longest wait behind it against #904's 500 ms bar (`WAIT bar ...`), and `rollover_parts` prints the
//! rollover's apply and each part of its log entry (`PARTS ...`). Run in a quiet CPU window:
//! `cargo test -p engine-node --lib sequencer::wait_bench::rollover -- --ignored --nocapture --test-threads=1`.

use engine_core::command::{Command, Sequenced};
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use sha2::{Digest, Sha256};

use crate::common::BTC;
use crate::common::launch_book::{Mix, NamedCounts, Origin, build, price_command};
use crate::history::derived::{Derived, RowsStore};
use crate::log::{CheckpointDomain, EncodedCommand, encode_events};
use crate::sequencer::wait_bench::mass::{Case, Outcome, accounts, roll_to, sequence, stream_market};
use crate::sequencer::wait_bench::time;
use crate::tee::EnclaveKey;

const NAMED: NamedCounts = NamedCounts { adders: 1, openers: 0, closeouts: 0 };
/// The stream's two prices, sent in turn so each moves the mark.
const PRICES: [&str; 2] = ["100", "100.01"];
/// The emission period close comes well after the book's own clock, which its assembly moves a few
/// minutes on, as the core's case sets it.
const START_AFTER_MS: i64 = 15 * 60_000;

/// The book's next daily rollover, a weekday's, with every benchmark's rate published a millisecond
/// before it and every market quoted at its own mark there, as a launch node's feeds keep them.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_rollover() {
    let mut book = build(&Mix::proposal(accounts()), NAMED);
    let stream = stream_market(&mut book);
    let due = book.h.engine.next_rollover().expect("a rollover is due");
    let gaps = book.h.engine.markets().values().filter_map(|market| market.weekend_window()).collect::<Vec<_>>();
    assert!(gaps.iter().all(|gap| gap.close != due), "the rollover is a weekday's, at no weekend gap's start");
    roll_to(&mut book, due);
    let at = due - 1;
    let marks: Vec<_> = book.h.engine.markets().iter().filter_map(|(id, m)| m.mark.map(|mark| (*id, mark))).collect();
    for (id, mark) in marks.into_iter().filter(|(id, _)| *id != stream) {
        let cmd = price_command(&book.h, id, &mark.to_string(), at);
        book.apply(cmd);
    }
    let positions: usize = book.h.engine.accounts().values().map(|account| account.positions.len()).sum();
    let prices = PRICES.map(|mid| price_command(&book.h, stream, mid, at));
    let case = Case { name: "rollover", market: BTC, at, step: true, event: Command::Tick, prices };
    let Outcome { financing, fills, .. } = sequence(book, case);
    assert!(financing * 100 >= positions * 99, "{financing} of {positions} positions financed, {fills} filled");
}

/// The close of emission period 0, where every account lost and each of its tiers' boost is read.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_period_close() {
    let mix = Mix {
        origin: Some(Origin { start_after: START_AFTER_MS, epoch_ms: 24 * 3_600_000 }),
        period_losses: true,
        ..Mix::proposal(accounts())
    };
    let mut book = build(&mix, NAMED);
    let stream = stream_market(&mut book);
    let end = book.h.engine.emissions().expect("emissions are initialised").schedule.period_end(0);
    let lost = book.h.engine.period_results().len();
    assert_eq!(lost, accounts() - book.named_total(), "every account lost");
    book.h.advance_to(end - 1);
    book.apply(Command::Tick);
    let at = end - 1;
    let prices = PRICES.map(|mid| price_command(&book.h, stream, mid, at));
    let case = Case { name: "period_close", market: BTC, at, step: true, event: Command::Tick, prices };
    let Outcome { emitted, .. } = sequence(book, case);
    let allocated = emitted.iter().filter(|e| matches!(e, Event::EmissionAllocated { .. })).count();
    assert_eq!(allocated, lost, "every account that lost is allocated");
}

/// The rollover's cost on the sequencer's thread in parts, on the same book at the latest version and
/// at the one before it, whose entry logs a row per position: the engine's apply with its derived rows,
/// then each part of the log entry timed alone on what the apply produced, as the sequencer builds it
/// (`Sequencer::log_command`): the events' one encoding, the seal's link and the seal, the command
/// digest, the line, and the segment's hash of the line.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn rollover_parts() {
    for version in [ProtocolVersion::V79, ProtocolVersion::LATEST] {
        let mut book = build(&Mix { version, ..Mix::proposal(accounts()) }, NAMED);
        let due = book.h.engine.next_rollover().expect("a rollover is due");
        roll_to(&mut book, due);
        let marks: Vec<_> =
            book.h.engine.markets().iter().filter_map(|(id, m)| m.mark.map(|mark| (*id, mark))).collect();
        for (id, mark) in marks {
            let cmd = price_command(&book.h, id, &mark.to_string(), due - 1);
            book.apply(cmd);
        }
        let seq = book.h.engine.last_seq().map_or(0, |seq| seq + 1);
        let sequenced = Sequenced { seq, ts: due, cmd: Command::Tick };
        let (apply_us, events) = time(|| book.h.engine.apply(&sequenced).expect("in sequence"));
        let (derive_us, derived) = time(|| Derived::of(seq, &events, book.h.engine.take_derived()).expect("rows"));
        let rows = derived.as_ref().map_or(0, |derived| derived.rows.len());
        let (encode_us, raw) = time(|| encode_events(&events).expect("encodes"));
        let mut encoded = EncodedCommand { sequenced: &sequenced, events: &raw, seal: None };
        let (link_us, link) = time(|| encoded.link(B256::ZERO).expect("links"));
        let key = EnclaveKey::test_key();
        let (seal_us, seal) =
            time(|| key.seal_command(CheckpointDomain { chain_id: 1, vault: AccountId([0; 20]) }, link));
        encoded.seal = Some(&seal);
        let (digest_us, _) = time(|| encoded.digest().expect("digests"));
        let (line_us, line) = time(|| encoded.line().expect("a line"));
        let (segment_us, _) = time(|| Sha256::digest(&line));
        let entry_us = encode_us + link_us + seal_us + digest_us + line_us + segment_us;
        println!(
            "PARTS rollover version={version:?} accounts={} events={} derived_rows={rows} entry_bytes={} \
             apply_us={apply_us} derive_us={derive_us} encode_us={encode_us} link_us={link_us} seal_us={seal_us} \
             digest_us={digest_us} line_us={line_us} segment_sha256_us={segment_us} entry_us={entry_us} \
             apply_and_entry_us={}",
            accounts(),
            events.len(),
            line.len(),
            apply_us + derive_us + entry_us
        );
    }
}

/// What a member's replica adds to a build over 30 rollovers, which store each rollover's rows
/// (`LiveReplay::store_rows_in`): the launch book's rollover rows stored at 30 seqs a day apart with no
/// file in place, and again with every file in place, where the replica skips each (`RowsStore::holds`).
/// The replica's replay is the same in both. Prints `REPLICA ...`.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn replica_rows() {
    const ROLLOVERS: u64 = 30;
    const DAY_MS: i64 = 24 * 3_600_000;
    let mut book = build(&Mix::proposal(accounts()), NAMED);
    let due = book.h.engine.next_rollover().expect("a rollover is due");
    roll_to(&mut book, due);
    let marks: Vec<_> = book.h.engine.markets().iter().filter_map(|(id, m)| m.mark.map(|mark| (*id, mark))).collect();
    for (id, mark) in marks {
        let cmd = price_command(&book.h, id, &mark.to_string(), due - 1);
        book.apply(cmd);
    }
    let seq = book.h.engine.last_seq().map_or(0, |seq| seq + 1);
    book.h.engine.apply(&Sequenced { seq, ts: due, cmd: Command::Tick }).expect("in sequence");
    let rows = book.h.engine.take_derived();
    let dir = crate::test_support::scratch_dir("replica-rows");
    let store = RowsStore::new(&dir, 400 * DAY_MS);
    let pass = || {
        time(|| {
            for day in 0..ROLLOVERS {
                let (at, ts) = (seq + day, due + i64::try_from(day).unwrap() * DAY_MS);
                if !store.holds(at, ts, &rows) {
                    store.record_unsynced(at, ts, &rows).expect("stored");
                }
            }
        })
        .0
    };
    let (empty_us, held_us) = (pass(), pass());
    println!(
        "REPLICA rows rollovers={ROLLOVERS} positions={} none_present_us={empty_us} all_present_us={held_us}",
        rows.len()
    );
}
