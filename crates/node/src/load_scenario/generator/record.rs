//! What the generator measures. Each client and each subscriber keeps its own samples, so no two
//! tasks wait on one lock, and the window's end takes them all.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::load_scenario::plan::{Figures, Quantiles};

/// One task's samples since the last window ended, or every task's once taken.
#[derive(Clone, Debug, Default)]
pub(super) struct Samples {
    pub sent: BTreeMap<String, u64>,
    pub replies: BTreeMap<String, u64>,
    pub fills: u64,
    pub order_to_fill_us: Vec<u64>,
    pub request_us: Vec<u64>,
    /// One subscriber's frame lags; once taken, each subscriber's, by its place.
    pub stream_lag_ms: Vec<u64>,
    pub by_subscriber: BTreeMap<usize, Vec<u64>>,
    /// The subscribers the samples were taken from.
    pub subscribers: usize,
    pub frames: u64,
    pub quote_frames: u64,
    pub disconnects: u64,
    pub late: u64,
    pub refused: u64,
    pub unanswered: u64,
}

/// One task's samples, shared with the window's end.
pub(super) type Recorder = Arc<Mutex<Samples>>;

impl Samples {
    /// Adds `other`'s samples to these.
    pub(super) fn merge(&mut self, other: Samples) {
        for (kind, count) in other.sent {
            *self.sent.entry(kind).or_default() += count;
        }
        for (kind, count) in other.replies {
            *self.replies.entry(kind).or_default() += count;
        }
        for (subscriber, lags) in other.by_subscriber {
            self.by_subscriber.entry(subscriber).or_default().extend(lags);
        }
        self.subscribers = self.subscribers.max(other.subscribers);
        self.fills += other.fills;
        self.order_to_fill_us.extend(other.order_to_fill_us);
        self.request_us.extend(other.request_us);
        self.stream_lag_ms.extend(other.stream_lag_ms);
        self.frames += other.frames;
        self.quote_frames += other.quote_frames;
        self.disconnects += other.disconnects;
        self.late += other.late;
        self.refused += other.refused;
        self.unanswered += other.unanswered;
    }

    /// What the samples show: the stream's lag over every frame, and the slowest subscriber's p99,
    /// which the launch bar holds every subscriber to.
    pub(super) fn figures(&self) -> Figures {
        let slowest = self.by_subscriber.values().map(|lags| Quantiles::of(&mut lags.clone()).p99).max();
        let silent = (0..self.subscribers).filter(|at| self.by_subscriber.get(at).is_none_or(Vec::is_empty)).count();
        Figures {
            sent: self.sent.clone(),
            replies: self.replies.clone(),
            fills: self.fills,
            order_to_fill_us: Quantiles::of(&mut self.order_to_fill_us.clone()),
            request_us: Quantiles::of(&mut self.request_us.clone()),
            stream_lag_ms: Quantiles::of(&mut self.stream_lag_ms.clone()),
            slowest_subscriber_p99_ms: slowest.unwrap_or(0),
            silent_subscribers: silent as u64,
            frames: self.frames,
            quote_frames: self.quote_frames,
            disconnects: self.disconnects,
            late: self.late,
            refused: self.refused,
            unanswered: self.unanswered,
        }
    }
}

/// Every client's and every subscriber's samples, each recorder emptied, with each subscriber's frame
/// lags kept apart by its place.
pub(super) fn take_all(clients: &[Recorder], subscribers: &[Recorder]) -> Samples {
    let mut all = Samples { subscribers: subscribers.len(), ..Samples::default() };
    for recorder in clients {
        all.merge(std::mem::take(&mut *recorder.lock().expect("a recorder")));
    }
    for (at, recorder) in subscribers.iter().enumerate() {
        let taken = std::mem::take(&mut *recorder.lock().expect("a recorder"));
        all.by_subscriber.insert(at, taken.stream_lag_ms.clone());
        all.merge(taken);
    }
    all
}

/// The stamp of the command a stream frame carries, the `ts` after its `"type":"command"`, whether
/// the frame is the message itself or a signed frame carrying it as an escaped string; none for any
/// other frame.
pub(super) fn stamp_of(frame: &str) -> Option<i64> {
    let (command, ts) = match frame.find(r#""type":"command""#) {
        Some(at) => (at, r#""ts":"#),
        None => (frame.find(r#"\"type\":\"command\""#)?, r#"\"ts\":"#),
    };
    let rest = &frame[command..];
    let digits = &rest[rest.find(ts)? + ts.len()..];
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    digits[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frames_stamp_is_read_from_the_message_or_from_a_signed_frame_carrying_it() {
        use engine_core::command::{Command, Sequenced};

        use crate::log::{CommandEntry, LogEntry, Offset};
        use crate::sequencer::StreamMsg;

        let sequenced = Sequenced { seq: 7, ts: 1_794_841_200_123, cmd: Command::Tick };
        let entry = LogEntry::Command(CommandEntry { sequenced, events: Some(Vec::new()), seal: None });
        let msg = StreamMsg::new(Offset::start_of(7), entry);
        let message = msg.json().unwrap();
        assert_eq!(stamp_of(message), Some(1_794_841_200_123), "{message}");
        let escaped = &msg.framed().unwrap().escaped;
        let signed = format!(r#"{{"v":1,"n":3,"time":1794841200200,"msg":{escaped},"sig":"0x00"}}"#);
        assert_eq!(stamp_of(&signed), Some(1_794_841_200_123), "{signed}");
        let heartbeat = r#"{"v":1,"n":4,"time":1,"msg":"{\"type\":\"heartbeat\",\"head\":null}","sig":"0x"}"#;
        assert_eq!(stamp_of(heartbeat), None);
        assert_eq!(stamp_of(r#"{"offset":{"seq":7,"index":1},"type":"checkpoint","seq":7}"#), None);
    }

    #[test]
    fn merged_samples_add_their_counts_and_keep_every_sample() {
        let one = Samples {
            fills: 2,
            order_to_fill_us: vec![5, 1],
            sent: [("market".into(), 2)].into(),
            ..Samples::default()
        };
        let two =
            Samples { fills: 1, order_to_fill_us: vec![3], sent: [("market".into(), 1)].into(), ..Samples::default() };
        let all = take_all(&[Recorder::new(one.into()), Recorder::new(two.into())], &[]);
        let figures = all.figures();
        assert_eq!((figures.fills, figures.sent["market"]), (3, 3));
        assert_eq!(
            (figures.order_to_fill_us.count, figures.order_to_fill_us.p50, figures.order_to_fill_us.max),
            (3, 3, 5)
        );
    }

    #[test]
    fn the_slowest_subscriber_sets_the_stream_figure_and_a_silent_one_is_counted() {
        let fast = Samples { stream_lag_ms: vec![1; 1_000], frames: 1_000, ..Samples::default() };
        let slow = Samples { stream_lag_ms: vec![150; 3], frames: 3, ..Samples::default() };
        let subscribers = [Recorder::new(fast.into()), Recorder::new(slow.into()), Recorder::default()];
        let mut leg = take_all(&[], &subscribers);
        // Pooled, the slow subscriber's frames are under 1% of the stream; alone, its p99 is 150 ms.
        assert_eq!(leg.figures().stream_lag_ms.p99, 1);
        assert_eq!((leg.figures().slowest_subscriber_p99_ms, leg.figures().silent_subscribers), (150, 1));
        let later = take_all(&[], &[Recorder::new(Samples { stream_lag_ms: vec![2], ..Samples::default() }.into())]);
        leg.merge(later);
        assert_eq!(leg.by_subscriber[&0].len(), 1_001, "a subscriber's lags across windows are its own");
        assert!(subscribers.iter().all(|recorder| recorder.lock().unwrap().frames == 0), "each recorder is emptied");
    }
}
