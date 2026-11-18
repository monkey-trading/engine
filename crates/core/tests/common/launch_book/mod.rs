//! A launch-scale book (#905): `Mix::accounts` accounts at `Mix::version` (the latest by default) on the
//! production genesis's 34 markets, each holding positions, resting orders up to its volume tier's
//! limit, a volume book, a toxicity score and session keys, dealt out as `Mix` says.
//!
//! The named accounts are made by signed commands, so they hold exactly what the engine gives a
//! trader. The rest are written into the engine's encoding around them, as the other 100k-account
//! fixtures do, from those accounts' own entries, so a field the engine adds is carried along. The
//! encoding is assembled as text and read once, since a tree of 100k accounts' values would take
//! several gigabytes.
//!
//! The 100k-account benchmarks of `hot_path_benchmark` price one market of a book in which nearly every
//! account holds one position; launch's accounts hold more, and a price visits each holder's whole
//! account. `Mix::proposal`, the one place to change the shares, deals out half the accounts one market,
//! 30% two to four, 15% five to twelve and 5% all 34, each market's holders in proportion to the cap
//! study's market mix (`research/cap-calibration/data/derived/market_mix.csv`), with $5,000 in each
//! market held; 70%, 20%, 8% and 2% at tiers 0 to 3, each tier above 0 with a full 30-day volume book
//! and tier 0 with the day its positions' opening notional counted on; and a fifth resting orders up to
//! their tier's limit, half inside a market's first spread band and half away from the price (buys at
//! half the mark, sells at twice it). Every account has a toxicity score, two session keys, the request
//! nonces and the credited reference they leave. Each is funded by a credited reference, as the mock
//! custody does, not by a custody receipt, so the book leaves out each account's receipt leaf and index
//! entry, about 4% of its state. Staking and emissions are initialised at
//! `ops/runbooks/production-deployment.toml`'s values, whose time origin is ahead of the book's clock,
//! so no epoch or period closes unless `Mix::origin` moves it. In-band orders buy where a market's id
//! is odd and sell where it is even, so the close-out case's fall in ETH-USD reaches none; a price that
//! reaches them is a mass fill, which `tests/launch_book.rs` checks happens and `mass_events_benchmark`
//! measures, and no timed case of `launch_book_benchmark` makes one.

mod build;
mod counts;
mod encode;
mod genesis;
mod mix;

// Each test binary uses a different subset.
#[allow(unused_imports)]
pub use build::{Book, CLOSEOUT_MARKET, MARK, NamedCounts, build, closeout_mark};
#[allow(unused_imports)]
pub use genesis::*;
#[allow(unused_imports)]
pub use mix::{Mix, Origin, Rng, Weak, WeakKind};
