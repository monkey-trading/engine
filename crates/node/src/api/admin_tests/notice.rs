//! The `V50` dev scenario: the stress parameters and a listed market's financing markup move only
//! after a seven-day notice, through `engine admin`, the signed admin route and TLS, on deterministic
//! time, and the public log replays to the same state.

use super::*;
use chrono::{DateTime, SecondsFormat, Utc};
use engine_core::operator_bounds::{NOTICE_MS, StressSettings};

use crate::clock::Clock;

impl Node {
    /// Moves the clock the sequencer stamps with and the API counts expiries from to `to`.
    async fn advance_to(&self, to: Millis) {
        self.stamps.advance(Duration::from_millis((to - self.stamps.now_ms()) as u64));
        let sample = sample_around(self.stamps.as_ref());
        self.state.tx.send(crate::sequencer::Request::TimeSample(sample)).await.unwrap();
    }

    /// `engine admin` with `rest` against this node at `url`, signing with the dev operator key at
    /// the node's time.
    async fn admin_at(&self, url: &str, rest: &[&str]) -> anyhow::Result<AdminRequest> {
        let key = KeyEnv { key_file: None, key: Some(HexKey(format!("0x{}", hex::encode(DEV_KEY)))) };
        let stamps = self.stamps.clone();
        admin::run_with_clock(self.args(url, rest), key, move || stamps.now_ms()).await
    }
}

/// Whether `result` is the engine's refusal for `reason`.
fn refused(result: anyhow::Result<AdminRequest>, reason: &str) {
    let err = result.expect_err(reason);
    assert!(err.to_string().contains(&format!("rejected the command: {reason}")), "{err:#}");
}

#[tokio::test]
async fn dev_scenario_stress_parameters_and_a_markup_move_only_after_a_seven_day_notice() {
    let mut node = Node::start_on("params-notice", true).await;
    node.state.clock = node.stamps.clone();
    let url = format!("https://{}", node.serve().await);
    let dir = scratch_dir("params-notice-files");
    fs::create_dir_all(&dir).unwrap();
    let mut wider = dev_config().genesis.params;
    let top = wider.stress_levels.len() - 1;
    wider.stress_levels[top].min_spread_bps += 1;
    let params = dir.join("params.toml");
    fs::write(&params, toml::to_string(&wider).unwrap()).unwrap();
    let mut btc = dev_config().genesis.markets.into_iter().find(|market| market.id == 1).unwrap();
    btc.financing.markup_bps += 25;
    let markup = btc.financing.markup_bps.to_string();
    let market = dir.join("btc.toml");
    fs::write(&market, toml::to_string(&btc).unwrap()).unwrap();
    let (params, market) = (params.to_str().unwrap(), market.to_str().unwrap());

    // 1. With no notice, both changes are refused.
    let no_notice = "needs a notice of at least seven days";
    refused(node.admin_at(&url, &["set-params", params]).await, &format!("a stress parameter change {no_notice}"));
    let reason = format!("market 1's markup change to {markup} {no_notice}");
    refused(node.admin_at(&url, &["upsert-market", market]).await, &reason);

    // 2. The operator announces both for seven days on.
    let due = NOW + NOTICE_MS;
    let at = DateTime::<Utc>::from_timestamp_millis(due).unwrap().to_rfc3339_opts(SecondsFormat::Secs, true);
    node.admin_at(&url, &["announce-params", "--stress-from", params, "--effective-at", &at]).await.unwrap();
    node.admin_at(&url, &["announce-markup", "1", &markup, "--effective-at", &at]).await.unwrap();

    // 3. A millisecond before then, both are still refused.
    node.advance_to(due - 1).await;
    refused(
        node.admin_at(&url, &["set-params", params]).await,
        &format!("the stress parameter change takes effect from {due}"),
    );
    let reason = format!("market 1's markup change to {markup} takes effect from {due}");
    refused(node.admin_at(&url, &["upsert-market", market]).await, &reason);

    // 4. From then, each applies once.
    node.advance_to(due).await;
    node.admin_at(&url, &["set-params", params]).await.unwrap();
    node.admin_at(&url, &["upsert-market", market]).await.unwrap();
    let (_, markets) = node.get("/v1/markets").await;
    let listed = markets.as_array().unwrap().iter().find(|m| m["id"] == 1).expect("market 1 is listed");
    assert_eq!(listed["financing"]["markup_bps"].to_string(), markup, "{markets}");

    // 5. The log holds each command with the operator's signature, and replays to the same state.
    let entries = node.entries();
    let logged = admin_entries(&entries);
    let applied = logged.iter().filter(|(_, events)| events.last() == Some(&Event::AdminApplied)).count();
    assert_eq!((logged.len(), applied), (8, 4), "{logged:?}");
    let replayed = replay(&entries).unwrap().engine;
    assert_eq!(StressSettings::of(replayed.params().unwrap()), StressSettings::of(&wider));
    assert_eq!(replayed.markets()[&1].spec.financing.markup_bps.to_string(), markup);
    let bounds = replayed.operator_bounds();
    assert!(bounds.notice.is_none() && bounds.markups.is_empty(), "{bounds:?}");
}
