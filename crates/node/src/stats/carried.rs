//! The dashboard record as a daily snapshot carries it (`Record`): every part a replay of the log rebuilds and the
//! views read, as it stands at the snapshot's checkpoint. The sequencer takes it there (`Stats::capture`), a clone
//! that the scans then in flight on the worker complete as they are installed, so it holds what a replay to the
//! checkpoint holds; the snapshot writer's thread waits for it and encodes it (`Capture::wait`). A start from the
//! snapshot restores it (`Stats::restore`) before its replay of the log after the checkpoint records into it, so
//! every answer reads as it did before the start. The latest scans are not carried, since a start ends with a scan
//! of its own (`Stats::finish_recovery`), and the retentions and ring sizes are the restoring node's settings.

use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

use alloy::primitives::I256;
use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::stats::activity::Activity;
use crate::stats::distributions::Distributions;
use crate::stats::record::sample_books;
use crate::stats::series::{Amount, Bucket, Buckets, Kind, Metric, SeriesStore};
use crate::stats::snapshot::Snapshot;
use crate::stats::turns::TurnHistory;
use crate::stats::{Begin, Distribution, Inner, Settings, Stats};

/// How often a wait for a capture installs the scans the worker has finished, which complete it.
const POLL: Duration = Duration::from_millis(20);

/// The dashboard record at a checkpoint.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    series: SeriesStore,
    activity: Activity,
    distributions: Distributions,
    turns: TurnHistory,
    begin: Option<Begin>,
    next_scan: Option<Millis>,
    next_light: Option<Millis>,
    queue_total: Fixed,
    #[serde(with = "text")]
    staked: u128,
    stakers: u64,
    distribution: Option<Distribution>,
}

/// A record taken at a checkpoint that waits for the scans then in flight, which are installed in it in order.
pub(super) struct Waiting {
    record: Record,
    /// The scans still to come before it is complete.
    scans: usize,
    reply: SyncSender<Record>,
}

/// The end a record taken at a checkpoint arrives on once complete (`Stats::capture`), and the record whose
/// installed scans complete it.
pub(crate) struct Capture {
    stats: Stats,
    taken: Receiver<Record>,
}

/// One series as a daily snapshot carries it: its metric's wire name, its market, and each kept resolution's
/// buckets, finest first, each its window start and its values as decimal strings: the open, high, low and close of
/// a level, the sum of a flow, the numerator and denominator of a ratio.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SeriesIn {
    metric: String,
    market: Option<MarketId>,
    buckets: [Vec<(Millis, Vec<AmountText>)>; 3],
}

/// Every series as a daily snapshot carries them, with the newest time recorded and the minute they were last
/// pruned at.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreIn {
    newest: Millis,
    pruned_minute: Millis,
    series: Vec<SeriesIn>,
}

/// A series value as a decimal string.
struct AmountText(Amount);

impl Serialize for SeriesStore {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct StoreOut<'a> {
            newest: Millis,
            pruned_minute: Millis,
            series: SeriesOut<'a>,
        }
        StoreOut { newest: self.newest, pruned_minute: self.pruned_minute, series: SeriesOut(self) }
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SeriesStore {
    /// The series a daily snapshot carries, kept for no retention until the restoring node's settings set one.
    /// Error cases:
    /// - the encoding is not one, or names a metric this build does not know, or a bucket of another shape than
    ///   its metric's
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let StoreIn { newest, pruned_minute, series } = StoreIn::deserialize(deserializer)?;
        let mut store = SeriesStore::new([None; 3]);
        (store.newest, store.pruned_minute) = (newest, pruned_minute);
        for SeriesIn { metric: name, market, buckets } in series {
            let metric = Metric::parse(&name).ok_or_else(|| serde::de::Error::custom(format!("no metric {name}")))?;
            let mut kept = Buckets::default();
            for (slot, buckets) in buckets.into_iter().enumerate() {
                for (ts, values) in buckets {
                    let bucket = bucket(metric, values.into_iter().map(|value| value.0).collect())
                        .ok_or_else(|| serde::de::Error::custom(format!("a {name} bucket at {ts} of another shape")))?;
                    kept[slot].insert(ts, bucket);
                }
            }
            store.series.insert((metric, market), kept);
        }
        Ok(store)
    }
}

impl Serialize for AmountText {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AmountText {
    /// Error cases:
    /// - the value is not a decimal string of a series value
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        I256::from_dec_str(&text).map(Self).map_err(serde::de::Error::custom)
    }
}

/// Every series of a store, streamed as `SeriesIn` reads them.
struct SeriesOut<'a>(&'a SeriesStore);

impl Serialize for SeriesOut<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct One<'a> {
            metric: &'static str,
            market: Option<MarketId>,
            buckets: [Slot<'a>; 3],
        }
        /// One resolution's buckets.
        struct Slot<'a>(&'a std::collections::BTreeMap<Millis, Bucket>);
        impl Serialize for Slot<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_seq(self.0.iter().map(|(ts, bucket)| (ts, Values(*bucket))))
            }
        }
        serializer.collect_seq(self.0.series.iter().map(|((metric, market), buckets)| One {
            metric: metric.name(),
            market: *market,
            buckets: [Slot(&buckets[0]), Slot(&buckets[1]), Slot(&buckets[2])],
        }))
    }
}

/// A bucket's values, as a daily snapshot carries them.
struct Values(Bucket);

impl Serialize for Values {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let values = match self.0 {
            Bucket::Level { open, high, low, close } => vec![open, high, low, close],
            Bucket::Sum(sum) => vec![sum],
            Bucket::Ratio { numerator, denominator } => vec![numerator, denominator],
        };
        serializer.collect_seq(values.into_iter().map(AmountText))
    }
}

/// The bucket of `metric` that `values` give, or `None` when they are not of its shape.
fn bucket(metric: Metric, values: Vec<Amount>) -> Option<Bucket> {
    Some(match (metric.kind(), values.as_slice()) {
        (Kind::Level, &[open, high, low, close]) => Bucket::Level { open, high, low, close },
        (Kind::Flow, &[sum]) => Bucket::Sum(sum),
        (Kind::Ratio, &[numerator, denominator]) => Bucket::Ratio { numerator, denominator },
        _ => return None,
    })
}

impl Stats {
    /// The record as it stands after the command applied last, the one a checkpoint covers, once every scan that
    /// command and those before it made due is in it: those the worker has not finished yet complete it as they
    /// are installed, in order (`Capture::wait`).
    pub(crate) fn capture(&self) -> Capture {
        let (reply, taken) = sync_channel(1);
        let mut inner = self.lock();
        let record = inner.record();
        match inner.background.as_ref().map_or(0, |worker| worker.in_flight()) {
            // One slot, which nothing else fills, and the end is held below.
            0 => assert!(reply.try_send(record).is_ok(), "a capture's reply holds one record"),
            scans => inner.captures.push(Waiting { record, scans, reply }),
        }
        Capture { stats: self.clone(), taken }
    }

    /// Puts `record`, taken at a checkpoint, in place of everything recorded, under this node's retentions and ring
    /// sizes, for a replay of the log after that checkpoint to record into (`Replay::record_stats_continuing`).
    pub(crate) fn restore(&self, record: Record) {
        let mut restored = Inner::new(&self.settings);
        restored.recovering = true;
        restored.restore(record, &self.settings);
        *self.lock() = restored;
    }
}

impl Capture {
    /// The record, once the scans it waits for are in it, installing those the worker has finished meanwhile;
    /// `None` when it is not complete within `within`, or the record was started over first.
    pub(crate) fn wait(self, within: Duration) -> Option<Record> {
        let deadline = Instant::now() + within;
        loop {
            match self.taken.try_recv() {
                Ok(record) => return Some(record),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => {}
            }
            // Taking the lock installs the scans the worker has finished.
            drop(self.stats.lock());
            let left = deadline.saturating_duration_since(Instant::now());
            match self.taken.recv_timeout(left.min(POLL)) {
                Ok(record) => return Some(record),
                Err(RecvTimeoutError::Disconnected) => return None,
                Err(RecvTimeoutError::Timeout) if left > POLL => {}
                Err(RecvTimeoutError::Timeout) => return None,
            }
        }
    }
}

impl Inner {
    /// A copy of every part a replay rebuilds.
    fn record(&self) -> Record {
        Record {
            series: self.series.clone(),
            activity: self.activity.clone(),
            distributions: self.distributions.clone(),
            turns: self.turns.clone(),
            begin: self.begin,
            next_scan: self.next_scan,
            next_light: self.next_light,
            queue_total: self.queue_total,
            staked: self.staked,
            stakers: self.stakers,
            distribution: self.distribution,
        }
    }

    /// Takes `record`'s parts in place of these, under `settings`' retentions and ring sizes.
    fn restore(&mut self, record: Record, settings: &Settings) {
        let Record {
            mut series,
            mut activity,
            mut distributions,
            turns,
            begin,
            next_scan,
            next_light,
            queue_total,
            staked,
            stakers,
            distribution,
        } = record;
        series.keep(settings.retention);
        activity.keep(settings.activity_limit);
        distributions.keep(settings.distribution_limit);
        (self.series, self.activity, self.distributions, self.turns) = (series, activity, distributions, turns);
        (self.begin, self.next_scan, self.next_light) = (begin, next_scan, next_light);
        (self.queue_total, self.staked, self.stakers, self.distribution) = (queue_total, staked, stakers, distribution);
    }

    /// Installs `snapshot`, the next scan to be installed, in each record that waits for it, its books in the
    /// series if `sampled`, and hands over each record that waits for no more.
    pub(super) fn complete_captures(&mut self, snapshot: &Snapshot, sampled: bool) {
        for waiting in &mut self.captures {
            if sampled {
                sample_books(&mut waiting.record.series, snapshot);
            }
            waiting.scans -= 1;
        }
        let (complete, waiting): (Vec<Waiting>, _) =
            std::mem::take(&mut self.captures).into_iter().partition(|waiting| waiting.scans == 0);
        self.captures = waiting;
        complete.into_iter().for_each(Waiting::hand_over);
    }

    /// Hands over every record that waits, as it stands: the worker that would complete it stopped, and this record
    /// goes on without those scans too.
    pub(super) fn release_captures(&mut self) {
        std::mem::take(&mut self.captures).into_iter().for_each(Waiting::hand_over);
    }
}

impl Waiting {
    /// Sends the record to its capture's end, which may have stopped waiting.
    fn hand_over(self) {
        let _ = self.reply.try_send(self.record);
    }
}

/// A `u128` as a decimal string, so no reader of the snapshot loses precision.
pub(super) mod text {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Writes `value` as a decimal string.
    pub(crate) fn serialize<S: Serializer>(value: &u128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    /// Reads a value `serialize` wrote.
    /// Error cases:
    /// - the value is not a decimal string of a `u128`
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests;
