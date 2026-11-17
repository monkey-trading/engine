//! The commands that act on every holder of a market, or on every account, at once, on a launch-scale
//! book (#907): 100k accounts at `ProtocolVersion::LATEST` on the production genesis's 34 markets
//! (`common::launch_book`, dealt out by `Mix::proposal`), each case on a book of its own, since a mass
//! event changes the book it runs on. Each prints its time and what it emitted as
//! `MASS <case> us=<microseconds> events=<n> ...`, then its standing against #904's 500 ms bar as
//! `LAUNCH bar <case> ...`; a case over the bar prints `OVER`. CI holds each to a guard
//! well above the bar, except an activation and the first price after a relisting, which the owner accepted
//! above it (#904) and which are only reported. Run in a quiet CPU window:
//! `cargo test -p engine-core --test mass_events_benchmark --locked -- --ignored --nocapture --test-threads=1 mass_`
//! (`MASS_ACCOUNTS` runs a smaller book, `MASS_ORDERS` fewer resting orders in the tier-limit re-key books).

mod common;
#[allow(dead_code)]
#[path = "launch_book_benchmark/report.rs"]
mod report;
#[path = "mass_events_benchmark/support.rs"]
mod support;

#[path = "mass_events_benchmark/closeouts.rs"]
mod closeouts;
#[path = "mass_events_benchmark/closes.rs"]
mod closes;
#[path = "mass_events_benchmark/corporate.rs"]
mod corporate;
#[path = "mass_events_benchmark/protocol.rs"]
mod protocol;
