//! A launch-scale book (#905): `Mix::accounts` accounts at `Mix::version` (the latest by default) on the
//! production genesis's 34 markets, each holding positions, resting orders up to its volume tier's
//! limit, a volume book, a toxicity score and session keys, dealt out as `Mix` says.
//!
//! The named accounts are made by signed commands, so they hold exactly what the engine gives a
//! trader. The rest are written into the engine's encoding around them, as the other 100k-account
//! fixtures do, from those accounts' own entries, so a field the engine adds is carried along. The
//! encoding is assembled as text and read once, since a tree of 100k accounts' values would take
//! several gigabytes.

mod build;
mod counts;
mod genesis;
mod mix;

// Each test binary uses a different subset.
#[allow(unused_imports)]
pub use build::{Book, CLOSEOUT_MARKET, MARK, NamedCounts, build, closeout_mark};
#[allow(unused_imports)]
pub use genesis::*;
#[allow(unused_imports)]
pub use mix::{Mix, Origin, Weak, WeakKind};
