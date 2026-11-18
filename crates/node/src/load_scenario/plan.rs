//! What a run does, shared by the node's side and the load generator's: the legs and their rates,
//! the request mix, the launch bar each leg is judged by, and the lines the generator reports on.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Launch flow: the cap study's observed average of fills a second across markets
/// (`research/cap-calibration/README.md`, "Data").
pub(super) const LAUNCH_FILLS_PER_SECOND: f64 = 3.7;
/// BTC-USD's share of launch flow, #549's model of it (`research/rogue-operator/README.md`, "Data").
pub(super) const LAUNCH_BTC_FILLS_PER_SECOND: f64 = 1.5;

/// The request mix [proposal], in per cent of requests: market orders, limit orders, cancels, and
/// cancel-alls or close-alls, half each.
pub(super) const MARKET_ORDERS: u64 = 40;
pub(super) const LIMIT_ORDERS: u64 = 30;
pub(super) const CANCELS: u64 = 25;
pub(super) const BATCHES: u64 = 5;
const _: () = assert!(MARKET_ORDERS + LIMIT_ORDERS + CANCELS + BATCHES == 100, "the mix is every request");

/// What a trader account may send a second: below tier 0's rate of 2 (`volume_tiers`), so the rate
/// limit never refuses the plan's own requests.
pub(super) const ACCOUNT_RATE: f64 = 1.5;

/// #904's launch bar at ten times launch flow, confirmed by the owner on 17 November 2026.
pub(super) const ORDER_TO_FILL_US: [u64; 3] = [5_000, 25_000, 100_000];
pub(super) const PRICE_TO_STAMP_S: [f64; 3] = [0.001, 0.010, 0.050];
pub(super) const STREAM_LAG_P99_MS: u64 = 100;
pub(super) const BUSY_SHARE: f64 = 0.5;

/// The plan the node's side hands the generator, as JSON in `LOAD_PLAN`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Plan {
    /// The API's listener, reached through its trusted PROXY protocol peer.
    pub api: SocketAddr,
    /// The book's named key of the first trader (`common::launch_book::named_key`), and how many
    /// traders there are, each the next key.
    pub first_key: u32,
    pub traders: u32,
    /// Traders sharing one client address and its connection.
    pub accounts_per_client: u32,
    /// Stream subscribers, half of them signed, and the share of the signed ones holding quote
    /// subscriptions of `quote_markets` markets.
    pub subscribers: u32,
    pub quoting_per_cent: u32,
    pub quote_markets: usize,
    /// The measured legs, then the ramp if any.
    pub legs: Vec<Leg>,
    pub ramp: Option<Ramp>,
    /// Windows each leg reports separately, for the tolerance of a loaded runner.
    pub windows: u32,
}

/// One leg: a multiple of launch flow for a time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Leg {
    pub name: String,
    pub multiple: f64,
    pub secs: u64,
}

/// A ramp: legs from `from` times launch flow, each `factor` times the last, of `step_secs` each,
/// until a budget breaks or `max` is passed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Ramp {
    pub from: f64,
    pub factor: f64,
    pub step_secs: u64,
    pub max: f64,
}

/// A line the generator prints for the node's side, after `LOADGEN `.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "line", rename_all = "snake_case")]
pub(super) enum Line {
    /// Every trader holds a session key, and every subscriber is connected.
    Ready { traders: u32, subscribers: u32 },
    /// A leg's first request is about to go.
    Start { leg: String, multiple: f64 },
    /// A window of the leg has ended.
    Window { leg: String, index: u32, figures: Figures },
    /// The leg has ended and its replies are in.
    End { leg: String, multiple: f64, figures: Figures },
}

/// What the generator saw over a window or a leg.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Figures {
    /// Requests sent by kind, and replies by kind and outcome.
    pub sent: BTreeMap<String, u64>,
    pub replies: BTreeMap<String, u64>,
    pub fills: u64,
    /// A market order from its hand-out to its client to its filled reply, at the client, less the
    /// client's signing, in microseconds.
    pub order_to_fill_us: Quantiles,
    /// Every request from its hand-out to its reply, less the client's signing, in microseconds.
    pub request_us: Quantiles,
    /// A command frame's arrival at a subscriber after its stamp, in milliseconds, over every frame;
    /// the highest of the subscribers' own p99s, which the launch bar holds each to; and the
    /// subscribers that took no frame.
    pub stream_lag_ms: Quantiles,
    pub slowest_subscriber_p99_ms: u64,
    pub silent_subscribers: u64,
    pub frames: u64,
    pub quote_frames: u64,
    pub disconnects: u64,
    /// Requests the scheduler handed out over two milliseconds after their schedule.
    pub late: u64,
    /// Requests the node answered with anything but 200, or with a rejection, or that failed on the
    /// connection or its timeout.
    pub refused: u64,
    /// Requests with no reply once the leg's drain ended.
    pub unanswered: u64,
}

/// A distribution's size and quantiles.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub(super) struct Quantiles {
    pub count: u64,
    pub p50: u64,
    pub p99: u64,
    pub p999: u64,
    pub max: u64,
}

impl Quantiles {
    /// The quantiles of `samples`, each the sample at its rank (nearest rank, rounding up).
    pub(super) fn of(samples: &mut [u64]) -> Self {
        samples.sort_unstable();
        let at = |q: f64| {
            let rank = ((q * samples.len() as f64).ceil() as usize).clamp(1, samples.len());
            samples[rank - 1]
        };
        match samples.last() {
            None => Quantiles::default(),
            Some(max) => {
                Quantiles { count: samples.len() as u64, p50: at(0.5), p99: at(0.99), p999: at(0.999), max: *max }
            }
        }
    }
}

impl Plan {
    /// The legs to run: the measured ones, then the ramp's steps, which run until the node's side
    /// stops them.
    pub(super) fn steps(&self) -> Vec<Leg> {
        let mut steps = self.legs.clone();
        if let Some(ramp) = &self.ramp {
            let mut multiple = ramp.from;
            while multiple <= ramp.max {
                let secs = ramp.step_secs;
                steps.push(Leg { name: format!("ramp_{multiple}x"), multiple, secs });
                multiple *= ramp.factor;
            }
        }
        steps
    }

    /// The most launch flow's multiple the traders can send without any of them passing
    /// `ACCOUNT_RATE`.
    pub(super) fn ceiling(&self) -> f64 {
        f64::from(self.traders) * ACCOUNT_RATE / requests_per_second(1.0)
    }
}

/// Requests a second at `multiple` times launch flow: market orders, each a fill, at the flow's rate,
/// and the mix's other requests in proportion.
pub(super) fn requests_per_second(multiple: f64) -> f64 {
    LAUNCH_FILLS_PER_SECOND * multiple * 100.0 / MARKET_ORDERS as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_take_the_nearest_rank() {
        let mut samples: Vec<u64> = (1..=1_000).rev().collect();
        let q = Quantiles::of(&mut samples);
        assert_eq!((q.count, q.p50, q.p99, q.p999, q.max), (1_000, 500, 990, 999, 1_000));
        assert_eq!(Quantiles::of(&mut []).count, 0);
        let q = Quantiles::of(&mut [7]);
        assert_eq!((q.p50, q.p99, q.p999), (7, 7, 7));
    }

    #[test]
    fn launch_flow_sends_its_fills_as_two_fifths_of_its_requests() {
        assert!((requests_per_second(1.0) - 9.25).abs() < 1e-9);
        assert!((requests_per_second(10.0) - 92.5).abs() < 1e-9);
    }
}
