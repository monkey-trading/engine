//! The portfolio margin dev scenario: the account read gives each account's portfolio requirement
//! under the dev config's fixture group, BTC-USD and ETH-USD, beside its per-position margin, through
//! the signed order routes on deterministic time, each answer verified as `engine verify-response`
//! verifies it. A pair long BTC and short ETH needs less than its bands and at least the floor share of
//! them, BTC alone keeps its bands, both reads follow a 2% move in BTC, and once the pair's ETH leg
//! closes its BTC leg keeps its bands.

use engine_core::types::MarketId;

use super::spreads::{fill_event, price};
use super::*;
use crate::verify::{Expected, SentRequest, parse_challenge_arg, verify_response_files};

const BTC: MarketId = 1;
const ETH: MarketId = 2;

#[tokio::test(flavor = "multi_thread")]
async fn dev_scenario_a_hedged_pair_reads_a_portfolio_requirement_below_its_bands() {
    let mut node = Node::start("portfolio-margin-dev-scenario", true).await;
    serve_grid(&mut node, dev_config().portfolio_margin.expect("the dev config names a fixture group"));
    let (a, b) = (key(0x91), key(0x92));
    let (wallet_a, wallet_b) = (eip712::address(a.verifying_key()), eip712::address(b.verifying_key()));
    node.onboard(&a).await;
    node.onboard(&b).await;
    let mut answers = 0;

    // 1. A opens $1M long BTC and $1M short ETH; B opens $1M long BTC alone.
    price(&node, ETH, "4000").await;
    price(&node, BTC, "100000").await;
    fill_event(&node, &a, BTC, Side::Buy, "10", "100000").await;
    fill_event(&node, &a, ETH, Side::Sell, "250", "4000").await;
    fill_event(&node, &b, BTC, Side::Buy, "10", "100000").await;

    // 2. A's bands are $20,000. ETH loads 1.05, so the pair is $50k net short: 4.2% of it and 0.2% of
    // each leg is a worst loss of $6,100, and closing both legs at their 30bp ceilings costs $12,000
    // at the 50% close-out level: $18,100, above the $10,000 floor share. B's $1M of BTC alone loses
    // $44,000 and costs $6,000 to close, far above its $10,000 of bands, which it keeps.
    let read = verified(&node, &mut answers, wallet_a).await;
    assert_eq!(read["margin"], "20000", "{read}");
    assert_eq!(read["portfolio_margin"], requirement("18100", "9050", &[1, 2], "-50000", "6100", "up", "1900"));
    let read = verified(&node, &mut answers, wallet_b).await;
    assert_eq!(read["margin"], "10000", "{read}");
    assert_eq!(read["portfolio_margin"], requirement("10000", "5000", &[1], "1000000", "44000", "down", "-40000"));

    // 3. BTC rises 2%: A's BTC leg is $1.02M and its net $30k short; B's position is $1.02M.
    price(&node, BTC, "102000").await;
    price(&node, ETH, "4000").await;
    let read = verified(&node, &mut answers, wallet_a).await;
    assert_eq!(read["margin"], "20200", "{read}");
    assert_eq!(read["portfolio_margin"], requirement("17420", "8710", &[1, 2], "-30000", "5300", "up", "2780"));
    let read = verified(&node, &mut answers, wallet_b).await;
    assert_eq!(read["portfolio_margin"], requirement("10200", "5100", &[1], "1020000", "44880", "down", "-40800"));

    // 4. A closes its ETH leg: its BTC leg alone keeps its bands.
    fill_event(&node, &a, ETH, Side::Buy, "250", "4000").await;
    let read = verified(&node, &mut answers, wallet_a).await;
    assert_eq!(read["margin"], "10200", "{read}");
    assert_eq!(read["portfolio_margin"], requirement("10200", "5100", &[1], "1020000", "44880", "down", "-40800"));
}

#[tokio::test]
async fn a_grid_that_does_not_fit_the_listings_gives_no_portfolio_requirement_and_warns() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let mut node = Node::start("portfolio-margin-misfit", true).await;
    // EUR-USD, an FX market on its own hours, in the crypto group.
    let mut grid = dev_config().portfolio_margin.expect("the dev config names a fixture group");
    grid.groups[0].members[1].market = 10;
    serve_grid(&mut node, grid);
    let a = key(0x93);
    node.onboard(&a).await;
    let read = node.get(&format!("/v1/accounts/{}", eip712::address(a.verifying_key()))).await;
    assert_eq!((read.get("portfolio_margin"), &read["margin"]), (None, &json!("0")), "{read}");
    let warned =
        std::iter::from_fn(|| alerts.try_recv().ok()).find(|alert| alert["alert"] == "portfolio_grid_mismatch");
    let warned = warned.expect("the read raises the alert");
    assert_eq!(warned["level"], "WARN");
    assert!(warned["reason"].contains("markets 1 and 10 differ"), "{warned:?}");
}

/// Serves `node`'s API with `grid` as its `[portfolio_margin]`.
fn serve_grid(node: &mut Node, grid: engine_core::portfolio::PortfolioGrid) {
    node.state.portfolio_margin = Some(Arc::new(grid));
    node.app = router(node.state.clone()).layer(Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)))));
}

/// The read's `portfolio_margin`: the same requirement at the market's and at initial bands, as the
/// dev tiers have no margin floor, its close-out level's share, and the crypto group.
fn requirement(
    margin: &str,
    maintenance: &str,
    members: &[MarketId],
    net: &str,
    worst_loss: &str,
    direction: &str,
    offset: &str,
) -> Value {
    json!({
        "margin": margin,
        "initial_margin": margin,
        "maintenance_margin": maintenance,
        "groups": { "crypto": {
            "members": members,
            "net_notional": net,
            "worst_loss": worst_loss,
            "binding_direction": direction,
            "offset": offset,
        }},
    })
}

/// `account`'s read under a fresh challenge, verified as `engine verify-response` verifies a saved
/// answer: its headers and body written out, then checked against the enclave's key and the dev
/// deployment.
async fn verified(node: &Node, answers: &mut u8, account: AccountId) -> Value {
    *answers += 1;
    let path = format!("/v1/accounts/{account}");
    let challenge = format!("{:064x}", *answers);
    let request = Request::get(&path).header(engine_wire::CHALLENGE_HEADER, &challenge).body(Body::empty()).unwrap();
    let response = node.app.clone().oneshot(request).await.unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let saved: String =
        headers.iter().map(|(name, value)| format!("{name}: {}\r\n", value.to_str().unwrap())).collect();
    let (header_file, body_file) =
        (node.dir.join(format!("answer-{answers}.headers")), node.dir.join(format!("answer-{answers}.body")));
    std::fs::write(&header_file, format!("HTTP/1.1 {status}\r\n{saved}\r\n")).unwrap();
    std::fs::write(&body_file, &body).unwrap();
    let cfg = dev_config();
    let expected = Expected {
        signer: AccountId(EnclaveKey::test_key().address()),
        chain_id: cfg.custody.chain_id,
        vault: cfg.custody.vault.parse().unwrap(),
        challenge: parse_challenge_arg(&challenge).unwrap(),
    };
    let request = SentRequest { method: "GET", path_and_query: &path, body: &[] };
    verify_response_files(&expected, &request, &header_file, &body_file).expect("the answer verifies");
    serde_json::from_slice(&body).unwrap()
}
