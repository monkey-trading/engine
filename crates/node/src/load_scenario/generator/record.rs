//! What the generator measures. Each client and each subscriber keeps its own samples, so no two
//! tasks wait on one lock, and the window's end takes them all.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::load_scenario::plan::{Figures, Quantiles};

/// One task's samples since the last window ended.
#[derive(Clone, Debug, Default)]
pub(super) struct Samples {
    pub sent: BTreeMap<String, u64>,
    pub replies: BTreeMap<String, u64>,
    pub fills: u64,
    pub order_to_fill_us: Vec<u64>,
    pub request_us: Vec<u64>,
    pub stream_lag_ms: Vec<u64>,
    pub frames: u64,
    pub quote_frames: u64,
    pub disconnects: u64,
    pub late: u64,
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
        self.fills += other.fills;
        self.order_to_fill_us.extend(other.order_to_fill_us);
        self.request_us.extend(other.request_us);
        self.stream_lag_ms.extend(other.stream_lag_ms);
        self.frames += other.frames;
        self.quote_frames += other.quote_frames;
        self.disconnects += other.disconnects;
        self.late += other.late;
    }

    /// What the samples show.
    pub(super) fn figures(&self) -> Figures {
        Figures {
            sent: self.sent.clone(),
            replies: self.replies.clone(),
            fills: self.fills,
            order_to_fill_us: Quantiles::of(&mut self.order_to_fill_us.clone()),
            request_us: Quantiles::of(&mut self.request_us.clone()),
            stream_lag_ms: Quantiles::of(&mut self.stream_lag_ms.clone()),
            frames: self.frames,
            quote_frames: self.quote_frames,
            disconnects: self.disconnects,
            late: self.late,
        }
    }
}

/// Every recorder's samples, each recorder emptied.
pub(super) fn take_all(recorders: &[Recorder]) -> Samples {
    let mut all = Samples::default();
    for recorder in recorders {
        let taken = std::mem::take(&mut *recorder.lock().expect("a recorder"));
        all.merge(taken);
    }
    all
}

/// The stamp of the command a stream frame carries, from its `sequenced` object's `ts`, whether the
/// frame is the message itself or a signed frame carrying it as an escaped string; none for any
/// other frame.
pub(super) fn stamp_of(frame: &str) -> Option<i64> {
    let after = frame.find("sequenced")?;
    let rest = &frame[after..];
    let at = rest.find(r#""ts":"#).map(|at| at + 5).or_else(|| rest.find(r#"\"ts\":"#).map(|at| at + 7))?;
    let digits: &str = &rest[at..];
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    digits[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frames_stamp_is_read_from_the_message_or_from_a_signed_frame_carrying_it() {
        let message = r#"{"offset":{"seq":7,"index":0},"type":"command","sequenced":{"seq":7,"ts":1794841200123,"cmd":{"type":"tick"}},"events":[]}"#;
        assert_eq!(stamp_of(message), Some(1_794_841_200_123));
        let signed = format!(
            r#"{{"v":1,"n":3,"time":1794841200200,"msg":{},"sig":"0x00"}}"#,
            serde_json::to_string(message).unwrap()
        );
        assert_eq!(stamp_of(&signed), Some(1_794_841_200_123), "{signed}");
        assert_eq!(
            stamp_of(r#"{"v":1,"n":4,"time":1,"msg":"{\"type\":\"heartbeat\",\"head\":null}","sig":"0x"}"#),
            None
        );
        assert_eq!(stamp_of(r#"{"offset":{"seq":7,"index":1},"type":"checkpoint","seq":7}"#), None);
    }

    #[test]
    fn merged_samples_add_their_counts_and_keep_every_sample() {
        let mut all = Samples::default();
        let one = Samples {
            fills: 2,
            order_to_fill_us: vec![5, 1],
            sent: [("market".into(), 2)].into(),
            ..Samples::default()
        };
        let two =
            Samples { fills: 1, order_to_fill_us: vec![3], sent: [("market".into(), 1)].into(), ..Samples::default() };
        all.merge(one);
        all.merge(two);
        let figures = all.figures();
        assert_eq!((figures.fills, figures.sent["market"]), (3, 3));
        assert_eq!(
            (figures.order_to_fill_us.count, figures.order_to_fill_us.p50, figures.order_to_fill_us.max),
            (3, 3, 5)
        );
    }
}
