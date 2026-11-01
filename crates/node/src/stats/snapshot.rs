//! The scan of every account behind the house's position figures: open interest and margin, each
//! market's long and short, where accounts close out, and how far in profit their positions are.
//! One pass over the accounts, aggregates only: the snapshot names no account.

use std::collections::BTreeMap;

use engine_core::fixed::Rounding;
use engine_core::types::{MarketId, Millis};
use engine_core::{Engine, Fixed};
use serde::Serialize;

use crate::stats::closeout::{self, Reach, Standing};

/// A position map's bucketing: `half` buckets of `width` basis points each side of the centre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Grid {
    pub half: u32,
    pub width: u32,
}

/// One side of a bucket: USD of positions and how many.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Cell {
    pub usd: Fixed,
    pub count: u64,
}

/// A bucket: its long positions and its short ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Bin {
    pub long: Cell,
    pub short: Cell,
}

/// What the scan settings are, from `[stats]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    pub closeout: Grid,
    pub profit: Grid,
}

/// One market's share of a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarketBook {
    /// The mark the notionals are taken at.
    pub mark: Option<Fixed>,
    pub long: Cell,
    pub short: Cell,
    /// Positions whose accounts are below the close-out level at the marks already.
    pub due: Bin,
    /// Positions by the move from the mark that closes their account out, a fall below the centre
    /// bucket for a long, a rise above it for a short.
    pub closeout: Vec<Bin>,
    /// Positions no move inside the range closes out.
    pub beyond: Bin,
    /// Positions by their profit as a share of entry: the first and last bucket are open ended.
    pub profit: Vec<Bin>,
}

/// The figures of one scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub seq: u64,
    pub ts: Millis,
    /// Notional of every position, long and short, in USD.
    pub open_interest: Fixed,
    /// The margin the accounts holding positions require.
    pub open_margin: Fixed,
    pub open_positions: u64,
    /// Whether the margin and the position maps were taken, which a light scan skips.
    pub full: bool,
    /// What an epoch close would distribute at this scan, once staking is initialised.
    pub distribution_excess: Option<Fixed>,
    pub markets: BTreeMap<MarketId, MarketBook>,
}

impl Grid {
    /// A grid reaching at least `range` basis points in buckets of `width`.
    pub(crate) fn new(range: u32, width: u32) -> Self {
        Self { half: range.div_ceil(width), width }
    }

    /// The farthest offset the grid covers each way.
    pub(crate) fn range_bps(self) -> u32 {
        self.half * self.width
    }

    /// The bucket of an offset from the centre inside the range, 0 to `2 * half - 1`.
    fn inner(self, offset_bps: i64) -> usize {
        let from_low = (offset_bps + i64::from(self.range_bps())).max(0);
        usize::try_from(from_low / i64::from(self.width)).unwrap_or(0).min(2 * self.half as usize - 1)
    }

    /// The bucket of a profit, the open-ended first and last included.
    fn profit_bucket(self, profit_bps: i64) -> usize {
        let range = i64::from(self.range_bps());
        if profit_bps < -range {
            0
        } else if profit_bps >= range {
            2 * self.half as usize + 1
        } else {
            1 + self.inner(profit_bps)
        }
    }
}

impl Bin {
    fn add(&mut self, long: bool, usd: Fixed) {
        let cell = if long { &mut self.long } else { &mut self.short };
        cell.usd += usd;
        cell.count += 1;
    }
}

impl Cell {
    /// The cell of a position of `usd`.
    fn add(&mut self, usd: Fixed) {
        self.usd += usd;
        self.count += 1;
    }
}

impl MarketBook {
    fn new(mark: Option<Fixed>, layout: Layout) -> Self {
        Self {
            mark,
            long: Cell::default(),
            short: Cell::default(),
            due: Bin::default(),
            closeout: vec![Bin::default(); 2 * layout.closeout.half as usize],
            beyond: Bin::default(),
            profit: vec![Bin::default(); 2 * layout.profit.half as usize + 2],
        }
    }
}

/// Scans every account of `engine` at its last command, `seq` and `ts`. A light scan (`full` false)
/// totals only each market's long and short and the open interest and positions, skipping the
/// accounts' equity and margin, the position maps and the distributable excess, as a replay does for
/// the scans nothing will serve. An account with a position in a market it cannot value, a quote
/// currency with no FX index, is left out.
pub(crate) fn scan(engine: &Engine, seq: u64, ts: Millis, layout: Layout, full: bool) -> Snapshot {
    let mut books: BTreeMap<MarketId, MarketBook> =
        engine.markets().iter().map(|(id, market)| (*id, MarketBook::new(market.mark, layout))).collect();
    let (mut open_interest, mut open_margin, mut open_positions) = (Fixed::ZERO, Fixed::ZERO, 0);
    let level_bps = engine.params().map(|params| params.closeout_level_bps);
    for account in engine.accounts().values() {
        let valued = account.positions.keys().all(|id| {
            engine
                .markets()
                .get(id)
                .is_some_and(|market| market.spec.quote_currency.is_usd() || market.fx_mark.is_some())
        });
        if account.positions.is_empty() || !valued {
            continue;
        }
        let (equity, margin) = if full { engine.equity_and_margin(account, None) } else { Default::default() };
        open_margin += margin;
        for (id, position) in &account.positions {
            let market = &engine.markets()[id];
            let price = market.mark.unwrap_or(position.entry);
            let notional = market.usd_at_mark(position.qty.abs().mul_rounded(price, Rounding::Up), Rounding::Up);
            let long = position.qty.is_positive();
            let book = books.get_mut(id).expect("a book exists for every listed market");
            open_interest += notional;
            open_positions += 1;
            if long {
                book.long.add(notional)
            } else {
                book.short.add(notional)
            }
            if !full {
                continue;
            }
            if !position.entry.is_zero() && market.mark.is_some() {
                let move_bps = (price - position.entry).bps_of(position.entry);
                let profit_bps = if long { move_bps } else { move_bps.saturating_neg() };
                book.profit[layout.profit.profit_bucket(profit_bps)].add(long, notional);
            }
            let reach = match level_bps.filter(|_| notional.is_positive()) {
                Some(level_bps) if market.spec.quote_currency.fx_market() == Some(*id) => {
                    let standing = Standing { equity, margin, level_bps };
                    let (quantity, range) = (position.qty.abs(), layout.closeout.range_bps());
                    closeout::reach_self_converting(standing, long, quantity, position.entry, price, range)
                }
                Some(level_bps) => closeout::reach(
                    Standing { equity, margin, level_bps },
                    closeout::Position { notional, long, spec: &market.spec },
                    layout.closeout.range_bps(),
                ),
                None => Reach::Beyond,
            };
            match reach {
                Reach::Due => book.due.add(long, notional),
                Reach::Beyond => book.beyond.add(long, notional),
                Reach::At(offset) => book.closeout[layout.closeout.inner(offset)].add(long, notional),
            }
        }
    }
    Snapshot {
        seq,
        ts,
        open_interest,
        open_margin,
        open_positions,
        full,
        distribution_excess: engine.distributable_excess().filter(|_| full),
        markets: books,
    }
}
