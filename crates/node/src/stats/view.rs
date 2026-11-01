//! The answers: `GET /v1/house`'s added figures, `/v1/monkey`, `/v1/house/positions`,
//! `/v1/stats/series` and `/v1/monkey/activity`, each read from the record and the engine at the
//! sequencer's last command.

use engine_core::fixed::Rounding;
use engine_core::types::{MarketId, Millis};
use engine_core::{Engine, Fixed};
use serde::Serialize;

use crate::config::VOLUME_WINDOW_HOURS;
use crate::history::marks::Resolution;
use crate::stats::activity::Entry as ActivityEntry;
use crate::stats::series::{Bucket, Entry, KEPT, Metric, Scope, entry};
use crate::stats::snapshot::{Bin, Cell, Grid, MarketBook};
use crate::stats::{Begin, Inner, Stats};

/// Entries per series page when the request sets no limit.
pub(crate) const DEFAULT_LIMIT: usize = 500;
/// Most entries per series page.
pub(crate) const MAX_LIMIT: usize = 2000;
/// Most activity entries per page.
pub(crate) const MAX_ACTIVITY_PAGE: usize = 200;

/// A series request.
pub(crate) struct SeriesQuery {
    pub metric: String,
    pub market: Option<MarketId>,
    pub resolution: String,
    pub from_ms: Option<Millis>,
    pub to_ms: Option<Millis>,
    pub limit: Option<usize>,
}

/// A series page read from the record, its buckets not yet formatted: the formatting happens in the
/// request's task, off the thread that reads the record.
pub struct SeriesRows {
    page: SeriesPage,
    metric: Metric,
    rows: Vec<(Millis, Bucket)>,
}

impl SeriesRows {
    /// The page with its buckets formatted.
    pub fn finish(self) -> SeriesPage {
        let Self { mut page, metric, rows } = self;
        page.entries = rows.into_iter().filter_map(|(ts, bucket)| entry(metric, ts, bucket)).collect();
        page
    }
}

/// One page of a series, oldest first.
#[derive(Serialize)]
pub struct SeriesPage {
    metric: &'static str,
    market: Option<MarketId>,
    resolution: &'static str,
    /// `level` (open, high, low, close), `flow` (a sum) or `ratio` (a quotient), the entries' shape.
    kind: &'static str,
    unit: &'static str,
    /// The sequenced time of the first command recorded: nothing earlier exists in this answer.
    begins_ms: Option<Millis>,
    begins_seq: Option<u64>,
    /// Whether recording began at the log's first command; false after a recovery from a snapshot.
    from_genesis: bool,
    /// The start of the oldest bucket this series still holds, once retention has pruned older ones.
    retained_from_ms: Option<Millis>,
    entries: Vec<Entry>,
    /// The `from_ms` of the next page; `None` when the window is exhausted.
    next: Option<Millis>,
    max_limit: usize,
}

/// The figures `GET /v1/house` adds to cash, NAV and gross exposure.
#[derive(Serialize)]
pub struct HouseFigures {
    /// The seq the scan figures were taken at, `None` before the first scan.
    seq: Option<u64>,
    /// The sequenced time of that scan.
    snapshot_ms: Option<Millis>,
    open_interest: Option<Fixed>,
    open_margin: Option<Fixed>,
    open_positions: Option<u64>,
    /// The house's unrealised result at the marks: NAV less cash.
    unrealised: Fixed,
    /// USD traded over the hourly buckets back to `volume_since_ms`.
    volume: Fixed,
    volume_since_ms: Millis,
    markets: Vec<HouseMarket>,
}

#[derive(Serialize)]
struct HouseMarket {
    market: MarketId,
    symbol: String,
    mark: Option<Fixed>,
    /// Clients' long and short notional at the scan, and their difference.
    long: Option<Fixed>,
    short: Option<Fixed>,
    net: Option<Fixed>,
    long_positions: Option<u64>,
    short_positions: Option<u64>,
    /// The house's unrealised result in the market at its mark.
    unrealised: Fixed,
    volume: Fixed,
}

/// A bucket's positions.
#[derive(Serialize)]
struct Counts {
    long_usd: Fixed,
    long_count: u64,
    short_usd: Fixed,
    short_count: u64,
}

#[derive(Serialize)]
struct CloseoutBucket {
    /// The move from the mark that closes the account out, in basis points: from inclusive, to
    /// exclusive, and the prices they make.
    from_bps: i64,
    to_bps: i64,
    from_price: Option<Fixed>,
    to_price: Option<Fixed>,
    #[serde(flatten)]
    positions: Counts,
}

#[derive(Serialize)]
struct ProfitBucket {
    /// A position's profit as a share of its entry, in basis points: from inclusive, to exclusive,
    /// and open at the ends.
    from_bps: Option<i64>,
    to_bps: Option<i64>,
    #[serde(flatten)]
    positions: Counts,
}

#[derive(Serialize)]
pub struct PositionsView {
    market: MarketId,
    symbol: String,
    /// The seq and time of the scan the figures are from.
    seq: u64,
    snapshot_ms: Millis,
    mark: Option<Fixed>,
    closeout_range_bps: u32,
    closeout_bucket_bps: u32,
    profit_range_bps: u32,
    profit_bucket_bps: u32,
    long: Cell,
    short: Cell,
    /// Positions whose accounts are below the close-out level at the marks already.
    due_now: Counts,
    closeout: Vec<CloseoutBucket>,
    /// Positions no move within the range closes out.
    beyond_range: Counts,
    profit: Vec<ProfitBucket>,
}

#[derive(Serialize)]
pub struct ActivityPage {
    entries: Vec<ActivityEntry>,
    /// The `before` for the page after this one, as a decimal string; `None` when none is held.
    #[serde(serialize_with = "next_text")]
    next: Option<u64>,
    begins_ms: Option<Millis>,
    begins_seq: Option<u64>,
    from_genesis: bool,
    /// Whether the feed has dropped older entries for room, and the time of the oldest it holds: a
    /// page that ends with `next` null is then the end of what is held, not of what happened.
    truncated: bool,
    retained_from_ms: Option<Millis>,
}

impl From<Bin> for Counts {
    fn from(bin: Bin) -> Self {
        Self {
            long_usd: bin.long.usd,
            long_count: bin.long.count,
            short_usd: bin.short.usd,
            short_count: bin.short.count,
        }
    }
}

impl Inner {
    pub(super) fn begins_at_genesis(&self) -> bool {
        self.begin.is_some_and(|begin| begin.seq == 0)
    }

    /// The USD traded in the hourly buckets back from the one `now` falls in, and where that
    /// window starts: no earlier than the record began.
    fn volume(&self, market: Option<MarketId>, now: Millis) -> (Fixed, Millis) {
        let from = Resolution::H1.bucket(now) - (VOLUME_WINDOW_HOURS as i64 - 1) * Resolution::H1.ms();
        let sum = self.series.sum(Metric::Volume, market, Resolution::H1, from, now);
        (
            Fixed::from_raw(i128::try_from(sum).unwrap_or(i128::MAX)),
            self.begin.map_or(from, |Begin { ts, .. }| from.max(ts)),
        )
    }
}

impl Stats {
    /// The house figures a scan and the series add, at the engine's last command.
    pub(crate) fn house(&self, engine: &Engine) -> HouseFigures {
        let inner = self.lock();
        let now = engine.now();
        let (volume, volume_since_ms) = inner.volume(None, now);
        let markets = engine
            .markets()
            .iter()
            .map(|(id, market)| {
                let book = inner.snapshot.as_ref().and_then(|snapshot| snapshot.markets.get(id));
                let client = market.mark.map_or(Fixed::ZERO, |mark| {
                    market.usd_at_mark(mark.mul(market.client_qty) - market.client_cost, Rounding::Up)
                });
                HouseMarket {
                    market: *id,
                    symbol: market.spec.symbol.clone(),
                    mark: market.mark,
                    long: book.map(|book| book.long.usd),
                    short: book.map(|book| book.short.usd),
                    net: book.map(|book| book.long.usd - book.short.usd),
                    long_positions: book.map(|book| book.long.count),
                    short_positions: book.map(|book| book.short.count),
                    unrealised: -client,
                    volume: inner.volume(Some(*id), now).0,
                }
            })
            .collect();
        let snapshot = inner.snapshot.as_ref();
        HouseFigures {
            seq: snapshot.map(|snapshot| snapshot.seq),
            snapshot_ms: snapshot.map(|snapshot| snapshot.ts),
            open_interest: snapshot.map(|snapshot| snapshot.open_interest),
            open_margin: snapshot.map(|snapshot| snapshot.open_margin),
            open_positions: snapshot.map(|snapshot| snapshot.open_positions),
            unrealised: engine.house_nav() - engine.house_cash(),
            volume,
            volume_since_ms,
            markets,
        }
    }

    /// A market's position map from the last scan.
    /// Error cases:
    /// - the market is not listed
    ///
    /// `Ok(None)` when no scan has run yet.
    pub(crate) fn positions(&self, engine: &Engine, market: MarketId) -> Result<Option<PositionsView>, String> {
        let state = engine.markets().get(&market).ok_or_else(|| format!("market {market} is not listed"))?;
        let inner = self.lock();
        let Some(snapshot) = &inner.snapshot else { return Ok(None) };
        let Some(book) = snapshot.markets.get(&market) else { return Ok(None) };
        let layout = self.settings.layout;
        Ok(Some(PositionsView {
            market,
            symbol: state.spec.symbol.clone(),
            seq: snapshot.seq,
            snapshot_ms: snapshot.ts,
            mark: book.mark,
            closeout_range_bps: layout.closeout.range_bps(),
            closeout_bucket_bps: layout.closeout.width,
            profit_range_bps: layout.profit.range_bps(),
            profit_bucket_bps: layout.profit.width,
            long: book.long,
            short: book.short,
            due_now: book.due.into(),
            closeout: closeout_buckets(book, layout.closeout),
            beyond_range: book.beyond.into(),
            profit: profit_buckets(book, layout.profit),
        }))
    }

    /// One page of a series.
    /// Error cases:
    /// - the metric or resolution is unknown, or `market` is missing, unlisted, or one the metric
    ///   does not take
    /// - the limit is outside 1 to `MAX_LIMIT`, or `from_ms` is after `to_ms`
    pub(crate) fn series(&self, engine: &Engine, query: &SeriesQuery) -> Result<SeriesRows, String> {
        let metric =
            Metric::parse(&query.metric).ok_or_else(|| format!("metric must be one of {}", Metric::names()))?;
        let resolution = Resolution::parse(&query.resolution)
            .filter(|resolution| KEPT.contains(resolution))
            .ok_or("resolution must be one of 1m, 1h, 1d")?;
        match (metric.scope(), query.market) {
            (Scope::Market, None) => return Err(format!("{} needs a market", metric.name())),
            (Scope::Global, Some(_)) => return Err(format!("{} takes no market", metric.name())),
            (_, Some(market)) if !engine.markets().contains_key(&market) => {
                return Err(format!("market {market} is not listed"));
            }
            _ => {}
        }
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err(format!("limit must be from 1 to {MAX_LIMIT}"));
        }
        let (from, to) = (query.from_ms.unwrap_or(i64::MIN), query.to_ms.unwrap_or(i64::MAX));
        if from > to {
            return Err("from_ms must not be after to_ms".to_owned());
        }
        let inner = self.lock();
        let (rows, next) = inner.series.page(metric, query.market, resolution, from, to, limit);
        let page = SeriesPage {
            metric: metric.name(),
            market: query.market,
            resolution: resolution.as_str(),
            kind: match metric.kind() {
                crate::stats::series::Kind::Level => "level",
                crate::stats::series::Kind::Flow => "flow",
                crate::stats::series::Kind::Ratio => "ratio",
            },
            unit: metric.unit().name(),
            begins_ms: inner.begin.map(|begin| begin.ts),
            begins_seq: inner.begin.map(|begin| begin.seq),
            from_genesis: inner.begins_at_genesis(),
            retained_from_ms: inner.series.retained_from(metric, query.market, resolution),
            entries: Vec::new(),
            next,
            max_limit: MAX_LIMIT,
        };
        Ok(SeriesRows { page, metric, rows })
    }

    /// The newest MONKEY events older than `before`, newest first.
    /// Error cases:
    /// - the limit is outside 1 to `MAX_ACTIVITY_PAGE`
    pub(crate) fn activity(&self, before: Option<u64>, limit: usize) -> Result<ActivityPage, String> {
        if !(1..=MAX_ACTIVITY_PAGE).contains(&limit) {
            return Err(format!("limit must be from 1 to {MAX_ACTIVITY_PAGE}"));
        }
        let inner = self.lock();
        let (entries, next) = inner.activity.page(before, limit);
        let (truncated, retained_from_ms) = inner.activity.retained();
        Ok(ActivityPage {
            entries,
            next,
            begins_ms: inner.begin.map(|begin| begin.ts),
            begins_seq: inner.begin.map(|begin| begin.seq),
            from_genesis: inner.begins_at_genesis(),
            truncated,
            retained_from_ms,
        })
    }
}

/// The close-out buckets of a market's book, with the prices their offsets make at its mark.
fn closeout_buckets(book: &MarketBook, grid: Grid) -> Vec<CloseoutBucket> {
    let width = i64::from(grid.width);
    let price = |offset: i64| book.mark.map(|mark| mark + mark.mul_bps(offset));
    book.closeout
        .iter()
        .enumerate()
        .map(|(index, bin)| {
            let from_bps = (index as i64 - i64::from(grid.half)) * width;
            let to_bps = from_bps + width;
            CloseoutBucket {
                from_bps,
                to_bps,
                from_price: price(from_bps),
                to_price: price(to_bps),
                positions: (*bin).into(),
            }
        })
        .collect()
}

/// The profit buckets of a market's book, the first and last open ended.
fn profit_buckets(book: &MarketBook, grid: Grid) -> Vec<ProfitBucket> {
    let (width, range) = (i64::from(grid.width), i64::from(grid.range_bps()));
    let last = book.profit.len() - 1;
    book.profit
        .iter()
        .enumerate()
        .map(|(index, bin)| {
            let from_bps = (index != 0).then(|| -range + (index as i64 - 1) * width);
            let to_bps = (index != last).then(|| -range + index as i64 * width);
            ProfitBucket { from_bps, to_bps, positions: (*bin).into() }
        })
        .collect()
}

/// Serializes an optional id as a decimal string.
fn next_text<S: serde::Serializer>(next: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
    match next {
        Some(id) => serializer.collect_str(id),
        None => serializer.serialize_none(),
    }
}
