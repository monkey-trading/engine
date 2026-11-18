//! The state root after a rollover the way the node takes it, with the tree detached (#925).

use engine_core::command::Command;
use engine_core::event::Event;

use crate::cases::{reprice_open_markets, touch};
use crate::common::launch_book::{Book, benchmark_batch};
use crate::report::{noted, rss_mib, timed, within};

/// The node's copy floor (`ROOT_BOUND / (2 * TOUCHED_WEIGHT)`, `sequencer/roots.rs`): the record's size above which the
/// sequencer copies leaf writes out between checkpoints.
const EXTRACT_FLOOR: usize = 1_000;
/// The entries the sequencer copies between looks at the clock, and the time of one piece.
const PIECE_ENTRIES: usize = 128;
const PIECE_US: u128 = 1_000;

/// One more daily rollover on the book, then its state root the way the node takes it with the tree
/// detached (#925): the sequencer copies the rollover's leaf writes out in pieces of a millisecond while
/// the record is above the floor, the worker writes each part, and the commit writes the rest. Prints
/// what each side spends, and the commit's wait from the backlog it started at. Runs after every case
/// that reads the engine's own root, since detaching the tree ends those.
pub fn root_parts(book: &mut Book, commit_bar_us: u128, commit_guard_us: u128) {
    let due = book.h.engine.next_rollover().expect("a rollover is due");
    book.h.advance_to(due - 1);
    let batch = benchmark_batch(&book.h);
    let events = book.operate(batch);
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    reprice_open_markets(book, due);
    // The root work below is this rollover's alone.
    book.h.engine.state_root();
    book.h.advance_to(due);
    let (events, _) = timed(|| book.apply(Command::Tick));
    let financed = events.iter().filter(|e| matches!(e, Event::Financing { .. })).count();
    let mut tree = book.h.engine.detach_tree().expect("the book's engine holds a tree");
    let backlog = book.h.engine.root_backlog();

    // The sequencer's side: pieces of about a millisecond, each copying parts of PIECE_ENTRIES entries.
    let mut deltas = Vec::new();
    let (mut entries, mut writes, mut copy_total, mut copy_longest, mut pieces) =
        (0usize, 0usize, 0u128, 0u128, 0usize);
    while book.h.engine.root_backlog() > EXTRACT_FLOOR {
        let before = book.h.engine.root_backlog();
        let began = std::time::Instant::now();
        while book.h.engine.root_backlog() > EXTRACT_FLOOR && began.elapsed().as_micros() < PIECE_US {
            let remaining = book.h.engine.root_backlog();
            let delta = book.h.engine.take_root_delta(PIECE_ENTRIES);
            entries += remaining - book.h.engine.root_backlog();
            writes += delta.len();
            deltas.push(delta);
        }
        let us = began.elapsed().as_micros();
        copy_total += us;
        copy_longest = copy_longest.max(us);
        pieces += 1;
        assert!(book.h.engine.root_backlog() < before, "a piece copies something");
    }

    // The worker's side: each part written into the tree's working version.
    let (mut apply_total, mut apply_longest) = (0u128, 0u128);
    for delta in deltas {
        let ((), us) = timed(|| delta.apply(&mut tree));
        apply_total += us;
        apply_longest = apply_longest.max(us);
    }
    println!(
        "LAUNCH root_parts financed={financed} start_backlog={backlog} pieces={pieces} entries_copied={entries} \
         leaf_writes={writes} copy_total_us={copy_total} copy_longest_us={copy_longest} \
         apply_total_us={apply_total} apply_longest_us={apply_longest} \
         apply_us_per_write={:.3} copy_us_per_entry={:.3}",
        apply_total as f64 / writes.max(1) as f64,
        copy_total as f64 / entries.max(1) as f64
    );

    // The commit at the bound: what the checkpoint waits for.
    let left = book.h.engine.root_backlog();
    let ((root, work, seq), commit_us) = timed(|| {
        let inputs = book.h.engine.commit_inputs();
        let seq = inputs.seq();
        let (root, work) = inputs.apply(&mut tree);
        (root, work, seq)
    });
    let (released, release_us) = timed(|| tree.release(seq, root));
    assert!(released, "the commit's version releases");
    let (freed, collect_us) = timed(|| tree.collect(usize::MAX));
    let (branches, leaves) = tree.nodes();
    println!(
        "LAUNCH root_commit commit_wait_us={commit_us} backlog={left} leaves={} nodes={} release_us={release_us} collect_us={collect_us} \
         freed={freed} tree_branches={branches} tree_leaves={leaves} resident_mib={}",
        work.leaves,
        work.nodes,
        rss_mib()
    );
    noted("root_commit_bar", commit_us, commit_bar_us);
    within("root_commit_guard", commit_us, commit_guard_us);
    commit_sweep(book, &mut tree);
}

/// Commits at growing backlogs on the detached tree, each after crediting that many accounts (two
/// entries and about two leaf writes each) with nothing copied out first, so the commit does all the
/// work: the figures `ROOT_BOUND` is set from.
fn commit_sweep(book: &mut Book, tree: &mut engine_core::merkle::tree::StateTree) {
    let mut next = 0u64;
    for (round, accounts) in [100usize, 250, 500, 1_000, 2_000, 4_000].into_iter().enumerate() {
        for _ in 0..accounts {
            let account = book.account(next);
            next += 1;
            touch(book, account, 100 + round);
        }
        let backlog = book.h.engine.root_backlog();
        let ((seq, root, len), us) = timed(|| {
            let inputs = book.h.engine.commit_inputs();
            let (seq, len) = (inputs.seq(), inputs.len());
            let (root, _) = inputs.apply(tree);
            (seq, root, len)
        });
        tree.release(seq, root);
        tree.collect(usize::MAX);
        println!("LAUNCH root_commit_sweep backlog={backlog} leaf_writes={len} commit_wait_us={us}");
    }
}
