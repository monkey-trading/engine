//! The bucketed series: each metric kept per resolution in a bounded map, pruned back from the
//! newest time recorded, and paged oldest first.

use std::collections::BTreeMap;

use engine_core::Fixed;
use engine_core::fixed::Rounding;
use engine_core::types::{MarketId, Millis};
use serde::Serialize;

use crate::history::marks::Resolution;

/// The resolutions the series keep, finest first; the position of a resolution here is its slot.
pub(crate) const KEPT: [Resolution; 3] = [Resolution::M1, Resolution::H1, Resolution::D1];

/// What a series measures. The wire name is the `metric` query value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Metric {
    Nav,
    Cash,
    WithdrawalQueue,
    TotalStaked,
    TotalAllocated,
    CumulativeDistributed,
    Long,
    Short,
    Volume,
    MonkeyPerLoss,
    DistributedPerEpoch,
}

/// How a metric's buckets combine the values recorded into them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A level sampled over time: open, high, low and close.
    Level,
    /// An amount that accrues: the sum.
    Flow,
    /// A ratio of two sums, the quotient served.
    Ratio,
}

/// The unit a metric's values are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unit {
    /// USD with nine decimals.
    Usd,
    /// MONKEY base units.
    Monkey,
    /// USDC base units.
    Usdc,
    /// MONKEY base units per USD of realised loss, with nine decimals.
    MonkeyPerUsd,
}

/// Whether a metric is per market.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Needs `market`.
    Market,
    /// Takes `market`, or covers every market without it.
    Either,
    /// Takes no `market`.
    Global,
}

impl Metric {
    const ALL: [Metric; 11] = [
        Metric::Nav,
        Metric::Cash,
        Metric::WithdrawalQueue,
        Metric::TotalStaked,
        Metric::TotalAllocated,
        Metric::CumulativeDistributed,
        Metric::Long,
        Metric::Short,
        Metric::Volume,
        Metric::MonkeyPerLoss,
        Metric::DistributedPerEpoch,
    ];

    /// The wire name.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Metric::Nav => "nav",
            Metric::Cash => "cash",
            Metric::WithdrawalQueue => "withdrawal_queue",
            Metric::TotalStaked => "total_staked",
            Metric::TotalAllocated => "total_allocated",
            Metric::CumulativeDistributed => "cumulative_distributed",
            Metric::Long => "long",
            Metric::Short => "short",
            Metric::Volume => "volume",
            Metric::MonkeyPerLoss => "monkey_per_loss",
            Metric::DistributedPerEpoch => "distributed_per_epoch",
        }
    }

    /// The metric a wire name gives.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|metric| metric.name() == text)
    }

    /// Every wire name, for the refusal that lists them.
    pub(crate) fn names() -> String {
        Self::ALL.map(Metric::name).join(", ")
    }

    pub(crate) const fn kind(self) -> Kind {
        match self {
            Metric::Volume | Metric::DistributedPerEpoch => Kind::Flow,
            Metric::MonkeyPerLoss => Kind::Ratio,
            _ => Kind::Level,
        }
    }

    pub(crate) const fn unit(self) -> Unit {
        match self {
            Metric::TotalStaked | Metric::TotalAllocated => Unit::Monkey,
            Metric::CumulativeDistributed | Metric::DistributedPerEpoch => Unit::Usdc,
            Metric::MonkeyPerLoss => Unit::MonkeyPerUsd,
            _ => Unit::Usd,
        }
    }

    pub(crate) const fn scope(self) -> Scope {
        match self {
            Metric::Long | Metric::Short => Scope::Market,
            Metric::Volume => Scope::Either,
            _ => Scope::Global,
        }
    }
}

impl Unit {
    /// The wire name.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Unit::Usd => "usd",
            Unit::Monkey => "monkey_base_units",
            Unit::Usdc => "usdc_base_units",
            Unit::MonkeyPerUsd => "monkey_base_units_per_usd",
        }
    }

    /// `raw` as the wire text: a nine-decimal number for the USD units, whole base units otherwise.
    pub(crate) fn text(self, raw: i128) -> String {
        match self {
            Unit::Usd | Unit::MonkeyPerUsd => Fixed::from_raw(raw).to_string(),
            Unit::Monkey | Unit::Usdc => raw.to_string(),
        }
    }
}

/// What one bucket holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bucket {
    Level { open: i128, high: i128, low: i128, close: i128 },
    Sum(i128),
    Ratio { numerator: i128, denominator: i128 },
}

/// One bucket as served.
#[derive(Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum Entry {
    Level { ts: Millis, open: String, high: String, low: String, close: String },
    Value { ts: Millis, value: String },
}

/// A series' buckets, one map per kept resolution.
type Buckets = [BTreeMap<Millis, Bucket>; 3];

/// Every series, with their retention.
pub(crate) struct SeriesStore {
    retention: [Option<Millis>; 3],
    newest: Millis,
    /// The minute the series were last pruned at.
    pruned_minute: Millis,
    series: BTreeMap<(Metric, Option<MarketId>), Buckets>,
}

impl SeriesStore {
    /// An empty store keeping each resolution for its retention, `None` for ever.
    pub(crate) fn new(retention: [Option<Millis>; 3]) -> Self {
        Self { retention, newest: Millis::MIN, pruned_minute: Millis::MIN, series: BTreeMap::new() }
    }

    /// Records a level sampled at `ts`.
    pub(crate) fn level(&mut self, metric: Metric, market: Option<MarketId>, ts: Millis, value: i128) {
        self.record(metric, market, ts, |bucket| match bucket {
            Some(Bucket::Level { high, low, close, .. }) => {
                *high = (*high).max(value);
                *low = (*low).min(value);
                *close = value;
            }
            _ => *bucket = Some(Bucket::Level { open: value, high: value, low: value, close: value }),
        });
    }

    /// Adds `amount` to the sum of the bucket `ts` falls in.
    pub(crate) fn add(&mut self, metric: Metric, market: Option<MarketId>, ts: Millis, amount: i128) {
        self.record(metric, market, ts, |bucket| match bucket {
            Some(Bucket::Sum(sum)) => *sum = sum.saturating_add(amount),
            _ => *bucket = Some(Bucket::Sum(amount)),
        });
    }

    /// Adds a numerator and denominator to the ratio of the bucket `ts` falls in.
    pub(crate) fn ratio(&mut self, metric: Metric, ts: Millis, numerator: i128, denominator: i128) {
        self.record(metric, None, ts, |bucket| match bucket {
            Some(Bucket::Ratio { numerator: n, denominator: d }) => {
                *n = n.saturating_add(numerator);
                *d = d.saturating_add(denominator);
            }
            _ => *bucket = Some(Bucket::Ratio { numerator, denominator }),
        });
    }

    /// Applies `update` to the bucket of each kept resolution that `ts` falls in, then prunes what
    /// has aged out of each, counting back from the newest time recorded.
    fn record(&mut self, metric: Metric, market: Option<MarketId>, ts: Millis, update: impl Fn(&mut Option<Bucket>)) {
        self.newest = self.newest.max(ts);
        let buckets = self.series.entry((metric, market)).or_default();
        for (slot, resolution) in KEPT.into_iter().enumerate() {
            let map = &mut buckets[slot];
            let mut bucket = map.get(&resolution.bucket(ts)).copied();
            update(&mut bucket);
            map.insert(resolution.bucket(ts), bucket.expect("an update leaves a bucket"));
        }
        let minute = self.newest.div_euclid(60_000);
        if minute != self.pruned_minute {
            self.pruned_minute = minute;
            self.prune();
        }
    }

    /// Drops the buckets of every series that fall before their resolution's retention, counted back
    /// from the newest time recorded, so a series nothing is recorded to still ages out.
    fn prune(&mut self) {
        for buckets in self.series.values_mut() {
            for (slot, retention) in self.retention.into_iter().enumerate() {
                let Some(retention) = retention else { continue };
                let cutoff = self.newest.saturating_sub(retention);
                while buckets[slot].first_key_value().is_some_and(|(start, _)| *start < cutoff) {
                    buckets[slot].pop_first();
                }
            }
        }
        self.series.retain(|_, buckets| buckets.iter().any(|map| !map.is_empty()));
    }

    /// Buckets of a series within [`from`, `to`], oldest first, at most `limit`, and the start of
    /// the first bucket left out. A bucket of a ratio with no denominator is left out. The buckets
    /// are copied as they are held, for the caller to format off the thread that holds the record.
    pub(crate) fn page(
        &self,
        metric: Metric,
        market: Option<MarketId>,
        resolution: Resolution,
        from: Millis,
        to: Millis,
        limit: usize,
    ) -> (Vec<(Millis, Bucket)>, Option<Millis>) {
        let Some(slot) = KEPT.iter().position(|kept| *kept == resolution) else { return (Vec::new(), None) };
        let Some(buckets) = self.series.get(&(metric, market)) else { return (Vec::new(), None) };
        if from > to {
            return (Vec::new(), None);
        }
        let mut found = buckets[slot].range(from..=to).filter(|(_, bucket)| bucket.is_served());
        let rows: Vec<(Millis, Bucket)> = found.by_ref().take(limit).map(|(ts, bucket)| (*ts, *bucket)).collect();
        (rows, found.next().map(|(ts, _)| *ts))
    }

    /// The start of the oldest bucket a series still holds at `resolution`.
    pub(crate) fn retained_from(
        &self,
        metric: Metric,
        market: Option<MarketId>,
        resolution: Resolution,
    ) -> Option<Millis> {
        let slot = KEPT.iter().position(|kept| *kept == resolution)?;
        let buckets = self.series.get(&(metric, market))?;
        buckets[slot].first_key_value().map(|(start, _)| *start)
    }

    /// The sum recorded in the buckets of `resolution` starting within [`from`, `to`].
    pub(crate) fn sum(
        &self,
        metric: Metric,
        market: Option<MarketId>,
        resolution: Resolution,
        from: Millis,
        to: Millis,
    ) -> i128 {
        let Some(slot) = KEPT.iter().position(|kept| *kept == resolution) else { return 0 };
        let Some(buckets) = self.series.get(&(metric, market)) else { return 0 };
        buckets[slot]
            .range(from..=to)
            .map(|(_, bucket)| match bucket {
                Bucket::Sum(sum) => *sum,
                _ => 0,
            })
            .fold(0i128, i128::saturating_add)
    }

    /// Each series' resolution slot as sorted `(metric, market, bucket start, text)` rows, for
    /// comparing two stores.
    #[cfg(test)]
    pub(crate) fn rows(&self) -> Vec<(Metric, Option<MarketId>, usize, Millis, String)> {
        let mut rows = Vec::new();
        for ((metric, market), buckets) in &self.series {
            for (slot, map) in buckets.iter().enumerate() {
                for (ts, bucket) in map {
                    rows.push((*metric, *market, slot, *ts, format!("{bucket:?}")));
                }
            }
        }
        rows
    }
}

impl Bucket {
    /// Whether the bucket has a value to serve: a ratio needs a denominator.
    fn is_served(&self) -> bool {
        !matches!(self, Bucket::Ratio { denominator, .. } if *denominator <= 0)
    }
}

/// A bucket as served, or `None` for a ratio whose quotient does not fit.
pub(crate) fn entry(metric: Metric, ts: Millis, bucket: Bucket) -> Option<Entry> {
    let unit = metric.unit();
    Some(match bucket {
        Bucket::Level { open, high, low, close } => Entry::Level {
            ts,
            open: unit.text(open),
            high: unit.text(high),
            low: unit.text(low),
            close: unit.text(close),
        },
        Bucket::Sum(sum) => Entry::Value { ts, value: unit.text(sum) },
        Bucket::Ratio { numerator, denominator } => {
            // Base units per USD with nine decimals: `numerator * 10^9 / (denominator / 10^9)`.
            let scale = Fixed::from_raw(1_000_000_000_000_000_000);
            let per =
                Fixed::from_raw(numerator).checked_mul_div(scale, Fixed::from_raw(denominator), Rounding::TowardZero);
            Entry::Value { ts, value: unit.text(per.filter(|_| denominator > 0)?.raw()) }
        }
    })
}
