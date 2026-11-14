//! The toxicity score's cost from `V54` at 100k accounts, each against `V52` running the same
//! commands: a fill, the scoring of due markouts and the state root after it, with every launch
//! market holding a full hour of markout-spread history; and one price triggering 20k stops.

use super::common::close_spread::trade;
use super::common::session_keys::funded_at;
use super::*;
use engine_core::event::Event;
use engine_core::toxicity::GRID_MS;

/// The budget for the state root after a price command scores 2,200 markouts and every launch
/// market's history takes a grid point.
const SCORED_ROOT_BUDGET_US: u128 = 100_000;

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn toxicity_fill_latency() {
    // Among 100k accounts, each scored an hour ago, with 2,000 markouts within their horizon in
    // BTC, a scored client opens and closes 0.01 BTC in turn: from `V54` each fill reads its decayed
    // score's tier, and each close matches the open quantity among the queued markouts. A horizon
    // later one price command samples every market's index and scores the markouts, and the state
    // root takes the scores, the markouts and every market's spread history.
    const QUEUED: usize = 2_000;
    for version in [ProtocolVersion::V52, ProtocolVersion::V54] {
        let (mut h, client, fillers) = scored_fixture(version, 100_000, None);
        let markouts: Vec<serde_json::Value> = fillers
            .iter()
            .take(QUEUED)
            .map(|account| {
                let mut markout = json!({
                    "due": h.ts + 5_000, "account": account, "market": BTC, "side": "buy", "price": "100000",
                    "notional": "1000"
                });
                if version >= ProtocolVersion::V54 {
                    markout["qty"] = json!("0.01");
                    markout["open"] = json!("0.01");
                }
                markout
            })
            .collect();
        let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
        state["markouts"] = json!(markouts);
        h.engine = serde_json::from_value(state).expect("benchmark fixture");
        // The recovered engine's indexes and tree, outside the samples.
        h.engine.restore_indexes();
        h.engine.state_root();
        let mut times = Vec::with_capacity(2 * SAMPLES);
        for sample in 0..SAMPLES {
            for side in [Side::Buy, Side::Sell] {
                let began = Instant::now();
                let events = trade(&mut h, &client, BTC, side, "0.01", "100000");
                times.push(began.elapsed().as_micros());
                assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{sample}: {events:?}");
            }
        }
        times.sort_unstable();
        let (p50, p99) = (times[times.len() / 2 - 1], times[times.len() * 99 / 100 - 1]);
        println!("LATENCY toxicity_fill_{version:?} accounts=100000 queued={QUEUED} p50_us={p50} p99_us={p99}");
        assert!(p99 <= BATCH_BUDGET_US, "a {version:?} fill's p99 {p99}us exceeds {BATCH_BUDGET_US}us");
        h.engine.state_root();
        h.advance(5_000);
        let began = Instant::now();
        h.price(BTC, "100000");
        let scored = began.elapsed().as_micros();
        println!("LATENCY toxicity_scoring_{version:?} markouts={} us={scored}", QUEUED + 2 * SAMPLES);
        assert!(scored <= 50_000, "scoring the due markouts took {scored}us");
        let began = Instant::now();
        h.engine.state_root();
        let root = began.elapsed().as_micros();
        let markets = h.engine.markets().len();
        println!(
            "LATENCY toxicity_scored_root_{version:?} markouts={} markets={markets} us={root}",
            QUEUED + 2 * SAMPLES
        );
        assert!(root <= SCORED_ROOT_BUDGET_US, "the {version:?} root after scoring took {root}us");
    }
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn toxicity_trigger_latency() {
    // One price triggers a resting stop buy of 0.01 BTC for each of 20k scored accounts: from `V54`
    // each fill reads its tier and takes from the account's opposite markouts within the horizon,
    // which a scan of the queue would make quadratic in the fills.
    const TRIGGERED: usize = 20_000;
    let mut took = Vec::new();
    for version in [ProtocolVersion::V52, ProtocolVersion::V54] {
        let (mut h, _, _) = scored_fixture(version, TRIGGERED, Some("100100"));
        h.advance(1);
        let began = Instant::now();
        let events = h.price(BTC, "100100");
        took.push(began.elapsed().as_micros());
        let fills = events.iter().filter(|e| matches!(e, Event::Fill(_))).count();
        assert_eq!(fills, TRIGGERED + 1, "every stop fills");
        println!("LATENCY toxicity_trigger_{version:?} fills={fills} us={}", took[took.len() - 1]);
    }
    // Twice `V52`'s, and 10 ms over: a quadratic match takes seconds here.
    assert!(took[1] <= 2 * took[0] + 10_000, "V54's triggered fills took {}us against V52's {}us", took[1], took[0]);
}

/// `funded_at(version)` with `count` more accounts, each a copy of the client, scored an hour before
/// the sequenced time from `V54`, house capital of 500M, and BTC copied into every launch market
/// but the harness's, each priced 200 ms before a grid point and holding a full hour of
/// markout-spread history; with `stop`, the client and each copy rest a stop buy of 0.01 BTC at that
/// price. Returns the copies' account keys.
pub(super) fn scored_fixture(
    version: ProtocolVersion,
    count: usize,
    stop: Option<&str>,
) -> (Harness, Client, Vec<String>) {
    let (mut h, client, _) = funded_at(version);
    h.advance_to((h.ts.div_euclid(GRID_MS) + 2) * GRID_MS - 200);
    h.price(BTC, "100000");
    if let Some(price) = stop {
        rest(&mut h, &client, BTC, Side::Buy, "0.01", OrderKind::Stop { price: f(price) });
    }
    let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
    let key_of = |account: AccountId| serde_json::to_value(account).unwrap().as_str().unwrap().to_owned();
    let client_key = key_of(client.account);
    let template = state["accounts"][&client_key].clone();
    let eligibility = state["eligibility"][&client_key].clone();
    let order = template["orders"].as_object().and_then(|orders| orders.values().next().cloned());
    let step = h.ts / 3_600_000 - 1;
    let score =
        json!({"edge": "40000", "weight_squares": "50000000", "fills": "25", "notional": "25000", "step": step});
    let mut next_order = state["next_order_id"].as_u64().expect("an order id");
    let mut keys = Vec::new();
    for index in 0..count as u64 {
        let mut address = [0xdd; 20];
        address[12..].copy_from_slice(&(index + 1).to_be_bytes());
        let key = key_of(AccountId(address));
        let mut copy = template.clone();
        copy["positions"] = json!({});
        copy["toxicity_bps"] = json!(2);
        if let Some(order) = &order {
            let mut order = order.clone();
            order["order_id"] = json!(next_order);
            copy["orders"] = json!({next_order.to_string(): order});
            state["markets"][BTC.to_string()]["resting"][next_order.to_string()] = json!(key);
            next_order += 1;
        }
        state["accounts"][&key] = copy;
        state["eligibility"][&key] = eligibility.clone();
        if version >= ProtocolVersion::V54 {
            state["toxicity_scores"][&key] = score.clone();
        }
        keys.push(key);
    }
    state["next_order_id"] = json!(next_order);
    // House capital enough that no cap or utilisation level stops the copies' fills.
    state["house_cash"] = json!("500000000");
    let listed = state["markets"].as_object().expect("markets").len();
    for id in 1_000..(1_000 + LAUNCH_MARKETS.saturating_sub(listed)) {
        let mut copy = state["markets"][BTC.to_string()].clone();
        copy["spec"]["id"] = json!(id);
        copy["spec"]["symbol"] = json!(format!("COPY-{id}"));
        copy["resting"] = json!({});
        state["markets"][id.to_string()] = copy;
    }
    if version >= ProtocolVersion::V54 {
        state["toxicity_scores"][&client_key] = score;
        let end = h.ts / GRID_MS;
        let squares: Vec<_> = (end - 719..=end).map(|at| json!([at, "1.5"])).collect();
        let history = json!({"last": [end, "100000"], "squares": squares});
        for id in state["markets"].as_object().expect("markets").keys().cloned().collect::<Vec<_>>() {
            state["markout_spreads"][id] = history.clone();
        }
    }
    h.engine = serde_json::from_value(state).expect("benchmark fixture");
    h.engine.restore_indexes();
    (h, client, keys)
}
