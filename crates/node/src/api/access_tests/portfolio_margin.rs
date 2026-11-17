//! The portfolio margin dev scenario: the account read gives each account's portfolio requirement
//! under the dev config's fixture group, BTC-USD and ETH-USD, beside its per-position margin, through
//! the signed order routes on deterministic time, each answer verified as `engine verify-response`
//! verifies it. A pair long BTC and short ETH takes its matched exposure's credit, staying above the
//! group's floor share of its bands; BTC alone keeps its bands; both reads follow a 2% move in BTC; and
//! once the pair's ETH leg closes its BTC leg keeps its bands.

use engine_core::types::MarketId;

use super::spreads::{fill_event, price};
use super::*;
use crate::api::account::PortfolioMargin;
use crate::verify::{Expected, SentRequest, parse_challenge_arg, verify_response_files};

const BTC: MarketId = 1;
const ETH: MarketId = 2;

/// A group's figures in the read: members, net, matched notional, worst loss, binding direction and
/// offset.
type Group<'a> = (&'a [MarketId], &'a str, &'a str, &'a str, &'a str, &'a str);

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

    // 2. A's bands are $20,000. ETH loads 1.25, so its $1.25M of loading-weighted exposure meets BTC's
    // $1M: BTC is matched whole and ETH in the ratio 0.8, $18,000 of bands. Each matched part's own 0.2%
    // move and the cost of closing it at its 30bp ceiling at the 50% level, 0.8% of $1.8M, is $14,400,
    // so the credit is $3,600, within half the matched bands. B's BTC alone matches nothing and keeps
    // its $10,000 of bands.
    let read = verified(&node, &mut answers, wallet_a).await;
    assert_eq!(read["margin"], "20000", "{read}");
    let pair = (&[BTC, ETH][..], "-250000", "2000000", "3600", "up", "3600");
    assert_eq!(read["portfolio_margin"], requirement("16400", "8200", pair));
    let read = verified(&node, &mut answers, wallet_b).await;
    assert_eq!(read["margin"], "10000", "{read}");
    assert_eq!(read["portfolio_margin"], requirement("10000", "5000", (&[BTC], "1000000", "0", "0", "down", "0")));

    // 3. BTC rises 2%: A's BTC leg is $1.02M, matching ETH in the ratio 0.816; B's position is $1.02M.
    price(&node, BTC, "102000").await;
    price(&node, ETH, "4000").await;
    let read = verified(&node, &mut answers, wallet_a).await;
    assert_eq!(read["margin"], "20200", "{read}");
    let pair = (&[BTC, ETH][..], "-230000", "2040000", "3672", "up", "3672");
    assert_eq!(read["portfolio_margin"], requirement("16528", "8264", pair));
    let lone = (&[BTC][..], "1020000", "0", "0", "down", "0");
    let read = verified(&node, &mut answers, wallet_b).await;
    assert_eq!(read["portfolio_margin"], requirement("10200", "5100", lone));

    // 4. A closes its ETH leg: its BTC leg alone keeps its bands.
    fill_event(&node, &a, ETH, Side::Buy, "250", "4000").await;
    let read = verified(&node, &mut answers, wallet_a).await;
    assert_eq!(read["margin"], "10200", "{read}");
    assert_eq!(read["portfolio_margin"], requirement("10200", "5100", lone));
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
    assert!(warned["message"].contains("markets 1 and 10 differ"), "{warned:?}");
}

#[test]
fn a_misfit_grid_warns_and_the_next_newer_read_it_fits_ends_the_warning() {
    let (_guard, mut alerts, mut resolutions) = crate::test_support::capture_alerts_and_resolutions();
    let portfolio = PortfolioMargin::new(dev_config().portfolio_margin.expect("the dev config names a fixture group"));
    let taken = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::test_support::Alert>| {
        std::iter::from_fn(|| rx.try_recv().ok()).count()
    };
    // The first read the grid fits ends an episode a run before may have left open, and only it.
    portfolio.report(Some(1), None);
    portfolio.report(Some(1), None);
    assert_eq!((taken(&mut alerts), taken(&mut resolutions)), (0, 1));
    // A misfit read warns, keyed by the name alone, and the next fitting read ends it, once.
    let misfit = Some("group crypto: markets 1 and 10 differ in asset class or schedule");
    portfolio.report(Some(2), misfit);
    portfolio.report(Some(2), misfit);
    portfolio.report(Some(3), None);
    portfolio.report(Some(3), None);
    assert_eq!((taken(&mut alerts), taken(&mut resolutions)), (2, 1));
    // A fitting read of an older engine, finishing after a misfit read of a newer one, ends nothing.
    portfolio.report(Some(5), misfit);
    portfolio.report(Some(4), None);
    assert_eq!((taken(&mut alerts), taken(&mut resolutions)), (1, 0));
    portfolio.report(Some(6), None);
    assert_eq!(taken(&mut resolutions), 1);
}

#[test]
fn a_node_started_without_a_grid_ends_an_episode_a_run_with_one_left_open() {
    let (_guard, mut alerts, mut resolutions) = crate::test_support::capture_alerts_and_resolutions();
    let mut cfg = dev_config();
    assert!(PortfolioMargin::configured(&cfg).is_some());
    assert!(std::iter::from_fn(|| resolutions.try_recv().ok()).next().is_none());
    cfg.portfolio_margin = None;
    assert!(PortfolioMargin::configured(&cfg).is_none());
    assert_eq!(std::iter::from_fn(|| resolutions.try_recv().ok()).count(), 1);
    assert!(alerts.try_recv().is_err());
}

/// Serves `node`'s API with `grid` as its `[portfolio_margin]`.
fn serve_grid(node: &mut Node, grid: engine_core::portfolio::PortfolioGrid) {
    node.state.portfolio_margin = Some(Arc::new(PortfolioMargin::new(grid)));
    node.app = router(node.state.clone()).layer(Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)))));
}

/// The read's `portfolio_margin`: the same requirement at the market's and at initial bands, as the
/// dev tiers have no margin floor, its close-out level's share, and the crypto group's members, net,
/// matched notional, worst loss, binding direction and offset.
fn requirement(margin: &str, maintenance: &str, group: Group<'_>) -> Value {
    let (members, net, matched, worst_loss, direction, offset) = group;
    json!({
        "margin": margin,
        "initial_margin": margin,
        "maintenance_margin": maintenance,
        "groups": { "crypto": {
            "members": members,
            "net_notional": net,
            "matched_notional": matched,
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
