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
    pub refused: u64,
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
        self.refused += other.refused;
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
            refused: self.refused,
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
