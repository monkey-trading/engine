//! The close-out index's memory on the launch book (#905, #912): the bytes its build keeps live,
//! counted by this binary's allocator, beside the engine's and its other derived indexes'. Prints
//! `LAUNCH closeout_index_memory ...`. Run apart:
//! `cargo test -p engine-core --test launch_book_memory --locked -- --ignored --nocapture`, with
//! `LAUNCH_BOOK_ACCOUNTS` for a smaller book.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use common::launch_book::{Mix, NamedCounts, build};
use engine_core::Engine;
use engine_core::command::{Command, Sequenced};

/// The system allocator, counting the bytes it holds.
struct Counting;

/// Bytes allocated and not yet freed.
static LIVE: AtomicIsize = AtomicIsize::new(0);

#[global_allocator]
static ALLOCATOR: Counting = Counting;

// SAFETY: each call passes its arguments to the system allocator unchanged.
unsafe impl GlobalAlloc for Counting {
    /// Allocates as the system does, counting the bytes.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: the caller's layout, as `GlobalAlloc::alloc` requires.
        unsafe { System.alloc(layout) }
    }

    /// Frees as the system does, counting the bytes.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: the caller's pointer and layout, as `GlobalAlloc::dealloc` requires.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[test]
#[ignore = "100k-account benchmark: run apart"]
fn launch_book_closeout_index_memory() {
    let accounts = std::env::var("LAUNCH_BOOK_ACCOUNTS").ok().map_or(100_000, |n| n.parse().expect("a count"));
    let book = build(&Mix::proposal(accounts), NamedCounts { adders: 1, openers: 0, closeouts: 0 });
    let text = serde_json::to_string(&book.h.engine).expect("encoding");
    drop(book);
    let live = || LIVE.load(Ordering::Relaxed);
    // One copy with the derived indexes a command builds, which reaches no close-out check: no index
    // moves at the same time with the same quotes.
    let before = live();
    let mut hot: Engine = serde_json::from_str(&text).expect("decoding");
    let decoded = live();
    let (seq, ts) = (hot.last_seq().expect("a log") + 1, hot.now());
    let events = hot.apply(&Sequenced { seq, ts, cmd: Command::Tick }).expect("in sequence");
    assert!(events.is_empty(), "{events:?}");
    let indexed = live();
    drop(hot);
    // One with them and the close-out index.
    let start = live();
    let mut both: Engine = serde_json::from_str(&text).expect("decoding");
    let decoded_again = live();
    both.restore_indexes();
    let restored = live();
    let positions: usize = both.accounts().values().map(|acct| acct.positions.len()).sum();
    let index = (restored - decoded_again) - (indexed - decoded);
    let mib = |bytes: isize| bytes as f64 / f64::from(1 << 20);
    println!(
        "LAUNCH closeout_index_memory accounts={accounts} positions={positions} engine_mib={:.1} \
         other_indexes_mib={:.1} closeout_index_mib={:.1} bytes_per_position={:.0}",
        mib(decoded - before),
        mib(indexed - decoded),
        mib(index),
        index as f64 / positions as f64,
    );
    assert_eq!(decoded - before, decoded_again - start, "the two copies decode alike");
}
