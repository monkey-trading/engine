//! The `V52` dev scenario: every fill pays its size spread on the open and the close, in tenths of a
//! bp, the signed quote stream steps through the tiers a close crosses, and the operator moves a
//! market's bands, and lists a market, only within the bounds, through the signed admin API over TLS
//! on deterministic time; the public log replays to the same state, and a restart recovers it.

use engine_core::command::AdminCommand;
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::{MarketSpec, Tier};
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice, StressSettings};
use engine_core::spread_tier::SpreadTier;
use engine_core::types::MarketId;
use engine_wire::{Connection, Deployment, FrameVerifier, SignedFrame};
use futures_util::{SinkExt, StreamExt};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::*;
use crate::admin::{NodeApi, sign, submit};
use crate::clock::Clock;
use crate::tls::{TlsIdentity, serve};

const BTC: MarketId = 1;
const EUR_USD: MarketId = 10;
/// A market the dev genesis does not list.
const SOL: MarketId = 3;
const CHALLENGE: &str = "5a1d0e3f9b2c7a48e6d1f05b3a9c2e718f0e5c2a9b7d41e6a3c0f19d2e84b7c6";

/// The signed admin route over TLS, pinned to the node's key, as `engine admin` reaches it.
pub(super) struct Operator {
    api: NodeApi,
}

impl Operator {
    /// Serves `node`'s API over TLS on a free loopback port.
    pub(super) async fn serving(node: &Node) -> Self {
        let tls = TlsIdentity::generate().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap()).parse().unwrap();
        let (acceptors, app, limits) = (tls.acceptors.clone(), router(node.state.clone()), node.state.limits.clone());
        tokio::spawn(serve(listener, acceptors, app, limits, dev_config().api));
        Self { api: NodeApi { url, pin: tls.public_key_sha256 } }
    }

    /// `command`, signed with the dev operator key and its next nonce, through the admin route.
    async fn send(&self, node: &Node, command: AdminCommand) -> Vec<Event> {
        let address = node.state.operator.address;
        let nonce = node.state.query(move |engine| engine.next_request_nonce(address)).await.unwrap();
        let key = SigningKey::from_slice(&[0x42; 32]).unwrap();
        let request = sign(&key, &node.state.operator, command, nonce, node.clock.now_ms() + 60_000);
        submit(&self.api, &request).await.unwrap()
    }

    pub(super) async fn applies(&self, node: &Node, command: AdminCommand) {
        let events = self.send(node, command).await;
        assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    }

    pub(super) async fn refuses(&self, node: &Node, command: AdminCommand, reason: &str) {
        let events = self.send(node, command).await;
        assert!(
            matches!(events.last(), Some(Event::AdminRejected { reason: got }) if got.contains(reason)),
            "{reason}: {events:?}"
        );
    }
}

/// A signed market order of `qty` for `client`, quoted at `quoted`, through the order route; the fill's
/// price.
async fn fill(node: &Node, client: &SigningKey, market: MarketId, side: Side, qty: &str, quoted: &str) -> Fixed {
    fill_event(node, client, market, side, qty, quoted).await["price"].as_str().unwrap().parse().unwrap()
}

/// `fill`'s order; the fill event.
pub(super) async fn fill_event(
    node: &Node,
    client: &SigningKey,
    market: MarketId,
    side: Side,
    qty: &str,
    quoted: &str,
) -> Value {
    let account = eip712::address(client.verifying_key());
    let nonce = node.state.query(move |engine| engine.next_request_nonce(account)).await.unwrap();
    let expires_at = node.clock.now_ms() + 60_000;
    let mut request = OrderRequest {
        account,
        client_id: nonce,
        market,
        side,
        qty: qty.parse().unwrap(),
        kind: OrderKind::Market { quoted: quoted.parse().unwrap(), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    };
    let hash = eip712::order_hash(&eip712::domain(31_337, VAULT), &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(client, &hash) });
    let (status, events) = node.post("/v1/orders", json!(request)).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let fill = events.as_array().unwrap().iter().find(|event| event["type"] == "fill");
    fill.unwrap_or_else(|| panic!("{side:?} {qty}: {events}")).clone()
}

/// Credits `amount` more to a declared `client`.
pub(super) async fn fund(node: &Node, client: &SigningKey, amount: i64) {
    let account = eip712::address(client.verifying_key());
    let deposit = node
        .state
        .query(move |engine| {
            let reference = format!("0xspreads:{account}:{amount}");
            crate::test_support::access::funding(
                engine,
                Some(account),
                Some(account),
                Fixed::from_int(amount),
                &reference,
            )
        })
        .await
        .unwrap();
    let events = node.state.submit(deposit).await.unwrap();
    assert!(matches!(events.as_slice(), [Event::Deposited { .. }]), "{events:?}");
}

async fn balance(node: &Node, client: &SigningKey) -> Fixed {
    let account = eip712::address(client.verifying_key());
    node.state.query(move |engine| engine.accounts()[&account].balance).await.unwrap()
}

/// Prices `market` at `mid` from both venues, stamped now; the events the prices give.
pub(super) async fn price(node: &Node, market: MarketId, mid: &str) -> Vec<Event> {
    let ts = node.clock.now_ms();
    let quotes =
        ["a", "b"].map(|source| SourceQuote { source: source.into(), mid: mid.parse().unwrap(), ts, contract: None });
    node.state.submit(Command::Prices { market, quotes: quotes.to_vec() }).await.unwrap()
}

/// `account`'s BTC quote from a fresh signed stream connection, verified frame by frame.
async fn quote(node: &Node, account: AccountId) -> Value {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = router(node.state.clone()).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await });
    let query = format!("signed=1&challenge={CHALLENGE}");
    let (mut ws, _) = connect_async(format!("ws://{addr}/v1/stream?{query}")).await.unwrap();
    let cfg = dev_config();
    let deployment =
        Deployment { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse::<AccountId>().unwrap().0 };
    let challenge: [u8; 32] = hex::decode(CHALLENGE).unwrap().try_into().unwrap();
    let connection = Connection::new(challenge, &format!("/v1/stream?{query}"));
    let mut verifier = FrameVerifier::new(EnclaveKey::test_key().address(), deployment, connection);
    ws.send(WsMessage::text(format!(r#"{{"op":"quotes","account":"{account}","markets":[{BTC}]}}"#))).await.unwrap();
    loop {
        let message = timeout(Duration::from_secs(10), ws.next()).await.expect("a frame").unwrap().unwrap();
        let WsMessage::Text(text) = message else { continue };
        let frame: SignedFrame = serde_json::from_str(&text).unwrap();
        assert_eq!(verifier.verify(&frame), Ok(()), "frame {}", frame.n);
        let msg: Value = serde_json::from_str(&frame.msg).unwrap();
        if msg["type"] == "quote" {
            return msg;
        }
    }
}

/// Checks `account`'s signed quote for selling BTC: its first steps are `qtys`, and each step's price
/// is no better than that of any smaller sell, so an order quoted at the step it takes fills.
async fn check_bid(node: &Node, account: AccountId, qtys: &[i64]) {
    let quote = quote(node, account).await;
    let field = |step: &Value, key: &str| -> Fixed { step[key].as_str().unwrap().parse().unwrap() };
    let bid: Vec<(Fixed, Fixed)> =
        quote["bid"].as_array().unwrap().iter().map(|step| (field(step, "qty"), field(step, "price"))).collect();
    let expected: Vec<Fixed> = qtys.iter().map(|qty| Fixed::from_int(*qty)).collect();
    assert_eq!(bid.iter().map(|(qty, _)| *qty).take(qtys.len()).collect::<Vec<_>>(), expected, "{quote}");
    for units in (1..=260).step_by(3) {
        let qty = Fixed::from_int(units);
        let Some(&(_, step)) = bid.iter().find(|(top, _)| *top >= qty) else { continue };
        let own = node.state.query(move |engine| engine.quote(account, BTC, Side::Sell, qty)).await.unwrap().unwrap();
        if own.declined.is_none() {
            assert!(step <= own.price, "selling {qty}: step {step} against {}", own.price);
        }
    }
}

/// `market`'s listing in force, with `edit` applied.
async fn relisting(node: &Node, market: MarketId, edit: impl FnOnce(&mut MarketSpec) + Send + 'static) -> MarketSpec {
    node.state
        .query(move |engine| {
            let mut spec = engine.markets()[&market].spec.clone();
            edit(&mut spec);
            spec
        })
        .await
        .unwrap()
}

fn with_first_band(rate: i64) -> impl FnOnce(&mut MarketSpec) + Send + 'static {
    move |spec| spec.spread_tiers[0] = SpreadTier::tenths(spec.spread_tiers[0].up_to, rate)
}

#[tokio::test(flavor = "multi_thread")]
async fn dev_scenario_every_fill_pays_its_spread_and_curves_move_only_after_notice() {
    let mut node = Node::start("spreads-dev-scenario", true).await;
    let operator = Operator::serving(&node).await;
    let (a, b) = (key(0x61), key(0x62));
    let (a_account, b_account) = (eip712::address(a.verifying_key()), eip712::address(b.verifying_key()));
    for client in [&a, &b] {
        node.onboard(client).await;
        fund(&node, client, 1_000_000).await;
    }
    let (a_before, b_before) = (balance(&node, &a).await, balance(&node, &b).await);

    // 1. A and B open opposite BTC positions of $1m, $5m and $12m in interleaved legs, the house's net
    // exposure never past one leg. BTC's bands are 2bp to $2m, 4bp to $10m and 8bp above, and from V59 a
    // fresh wallet's first band ends at a quarter of its bound, $500k: (BTC, the price each leg's band
    // charge gives a buy) at 100,000.
    let opens = [("10", "100030"), ("20", "100040"), ("20", "100040"), ("20", "100040"), ("20", "100040")];
    let more = [("20", "100060"), ("10", "100080")];
    let mirror = |price: &str| (Fixed::from_int(200_000) - price.parse::<Fixed>().unwrap()).to_string();
    for (n, (qty, buy)) in opens.iter().chain(&more).enumerate() {
        assert_eq!(fill(&node, &a, BTC, Side::Buy, qty, "100000").await, buy.parse().unwrap(), "A's leg {n}");
        assert_eq!(fill(&node, &b, BTC, Side::Sell, qty, "100000").await, mirror(buy).parse().unwrap(), "B's leg {n}");
        // 2. The signed quote stream at $1m, $5m and $12m: a sell first closes through the tiers the
        // long holds, then reverses through the tiers it opens, each at the wallet's own bounds.
        match n {
            0 => check_bid(&node, a_account, &[5, 15]).await,
            // The house's cap, 30% of its NAV, declines a reversal past $3m net, and the largest size
            // that fits, the whole close, takes a step before the first declined one.
            2 => check_bid(&node, a_account, &[45, 50, 55]).await,
            6 => check_bid(&node, a_account, &[20, 115, 120, 125]).await,
            _ => {}
        }
    }
    // Each close pays the band it closes through: $2m at 8bp, four at 4bp, and the last $1.5m at 4bp and
    // $500k at 2bp.
    for (n, (qty, sell)) in
        [("20", "99920"), ("20", "99960"), ("20", "99960"), ("20", "99960"), ("20", "99960"), ("20", "99965")]
            .into_iter()
            .enumerate()
    {
        assert_eq!(fill(&node, &a, BTC, Side::Sell, qty, "100000").await, sell.parse().unwrap(), "A's close {n}");
        assert_eq!(
            fill(&node, &b, BTC, Side::Buy, qty, "100000").await,
            mirror(sell).parse().unwrap(),
            "B's close {n}"
        );
    }
    // The staircase to $12m costs 5500 each way, and the round trip pays it twice, to within the
    // rounding of the averaged entry price each partial close realises against.
    for (account, before) in [(&a, a_before), (&b, b_before)] {
        let paid = before - balance(&node, account).await;
        assert!((paid - Fixed::from_int(11_000)).abs() < "0.000001".parse().unwrap(), "{paid}");
    }
    // A million euros at 1 pays 0.4bp, 40, each way.
    price(&node, EUR_USD, "1").await;
    let before = balance(&node, &a).await;
    assert_eq!(fill(&node, &a, EUR_USD, Side::Buy, "1000000", "1").await, "1.00004".parse().unwrap());
    assert_eq!(fill(&node, &a, EUR_USD, Side::Sell, "1000000", "1").await, "0.99996".parse().unwrap());
    assert_eq!(before - balance(&node, &a).await, Fixed::from_int(80));

    // 3. Below a floor, or changed without a notice, a band is refused, as is a multiplier below 1x.
    for (market, rate, reason) in [
        (BTC, 0, "below the 10 tenths floor"),
        (BTC, 5, "below the 10 tenths floor"),
        (BTC, 30, "market 1's spread band change needs a notice of at least seven days"),
        (EUR_USD, 0, "below the 1 tenths floor"),
    ] {
        let spec = relisting(&node, market, with_first_band(rate)).await;
        operator.refuses(&node, AdminCommand::UpsertMarket(spec), reason).await;
    }
    let mut params = dev_config().genesis.params;
    params.stress_levels[0].size_spread_multiplier_bps = 0;
    let stress = Some(StressSettings::of(&params));
    let due = node.clock.now_ms() + NOTICE_MS;
    let zero = ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress,
        profit_fee_curve: None,
        volume_tiers: None,
        effective_at: due,
        score_tiers: None,
    };
    let reason = "every stress level's size multiplier is at least 10000 bps";
    operator.refuses(&node, AdminCommand::AnnounceParams(zero), reason).await;

    // 4. A notice of a 3bp first band and a 1.5% first margin band applies from its time; the wallet
    // pays it to $500k and 4bp above.
    let noticed = relisting(&node, BTC, |spec| {
        spec.spread_tiers[0] = SpreadTier::tenths(spec.spread_tiers[0].up_to, 30);
        spec.margin_tiers[0] = Tier { bps: 150, ..spec.margin_tiers[0] };
    })
    .await;
    let notice = ListingNotice {
        spread_tiers: Some(noticed.spread_tiers.clone()),
        margin_tiers: Some(noticed.margin_tiers.clone()),
        markup_bps: None,
        effective_at: due,
    };
    operator.applies(&node, AdminCommand::AnnounceListing { market: BTC, notice }).await;
    node.move_time((NOTICE_MS - 1) as u64).await;
    operator.refuses(&node, AdminCommand::UpsertMarket(noticed.clone()), &format!("takes effect from {due}")).await;
    node.move_time(1).await;
    node.source.lock().unwrap().observed_at = node.clock.now_ms();
    operator.applies(&node, AdminCommand::UpsertMarket(noticed)).await;
    price(&node, BTC, "100000").await;
    assert_eq!(fill(&node, &a, BTC, Side::Buy, "10", "100000").await, "100035".parse().unwrap());
    assert_eq!(fill(&node, &a, BTC, Side::Sell, "10", "100000").await, "99965".parse().unwrap());

    // 5. A BTC ceiling below its launch close charge, 15bps and 3bp at 4x, is refused.
    let adoption = |market, bps, effective_at, listing| AdminCommand::AdoptCloseSpreadCeiling {
        market,
        max_close_spread_bps: bps,
        effective_at,
        listing,
    };
    let due = node.clock.now_ms() + NOTICE_MS;
    let reason = "market 1's close-spread ceiling of 26bps is below its launch close charge of 27.0bps";
    operator.refuses(&node, adoption(BTC, 26, due, None), reason).await;

    // 6. An adoption for SOL-USD binds its proposed listing: another listing is refused, as is the
    // proposal while a wider toxicity add-on lifts its charge, 23bps at launch, over the ceiling, and
    // then it lists. The top score tier moves only on a notice, to at most half again its 5bps.
    let proposal = relisting(&node, BTC, |spec| {
        spec.id = SOL;
        spec.symbol = "SOL-USD".into();
        spec.spread_tiers = spec.spread_tiers.iter().map(|tier| SpreadTier::tenths(tier.up_to, 20)).collect();
    })
    .await;
    operator.applies(&node, adoption(SOL, 24, due, Some(proposal.clone()))).await;
    let genesis = dev_config().genesis.params;
    let mut toxic = genesis.clone();
    let top = toxic.toxicity.score_tiers.len() - 1;
    toxic.toxicity.score_tiers[top].extra_spread_bps = 7;
    let tiers_notice = |params: &engine_core::params::ProtocolParams, effective_at| {
        let score_tiers = Some(params.toxicity.score_tiers.clone());
        let notice = ParamsNotice {
            profit_fee_bps: None,
            closeout_level_bps: None,
            stress: None,
            score_tiers,
            effective_at,
            profit_fee_curve: None,
            volume_tiers: None,
        };
        AdminCommand::AnnounceParams(notice)
    };
    operator.applies(&node, tiers_notice(&toxic, node.clock.now_ms() + NOTICE_MS)).await;
    node.move_time(NOTICE_MS as u64).await;
    let renamed = MarketSpec { symbol: "SOL-PERP".into(), ..proposal.clone() };
    let reason = "market 3's first listing must be the listing its ceiling adoption bound";
    operator.refuses(&node, AdminCommand::UpsertMarket(renamed), reason).await;
    operator.applies(&node, AdminCommand::SetParams(toxic)).await;
    let reason = "market 3's close-spread ceiling of 24bps is below its launch close charge of 25.0bps";
    operator.refuses(&node, AdminCommand::UpsertMarket(proposal.clone()), reason).await;
    operator.applies(&node, tiers_notice(&genesis, node.clock.now_ms() + NOTICE_MS)).await;
    node.move_time(NOTICE_MS as u64).await;
    operator.applies(&node, AdminCommand::SetParams(genesis)).await;
    operator.applies(&node, AdminCommand::UpsertMarket(proposal.clone())).await;

    // 7. The public log replays to the same state, and a restart recovers the root it replays to.
    let mut replayed = replay(&node.entries()).unwrap().engine;
    let live = node
        .state
        .query(move |engine| {
            let balances = [a_account, b_account].map(|account| engine.accounts()[&account].balance);
            (balances, engine.markets()[&BTC].spec.clone(), engine.markets()[&SOL].spec.clone())
        })
        .await
        .unwrap();
    let balances = [a_account, b_account].map(|account| replayed.accounts()[&account].balance);
    assert_eq!((balances, replayed.markets()[&BTC].spec.clone(), replayed.markets()[&SOL].spec.clone()), live);
    assert_eq!(replayed.close_spread_ceilings().unwrap()[&SOL], 24);
    let policy = node.state.query(|engine| engine.access_policy().cloned()).await.unwrap();
    let (operator, clock, dir) = (node.state.operator, node.clock.clone(), node.dir.clone());
    let sequencer = node.tasks.remove(1);
    sequencer.abort();
    let _ = sequencer.await;
    drop(node);
    let cfg = crate::sequencer::SequencerConfig {
        access_policy: policy,
        chain_id: 31_337,
        vault: VAULT,
        operator,
        clock,
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let enclave = Arc::new(EnclaveKey::test_key());
    let mut recovered =
        Sequencer::recover(cfg, enclave, broadcast::channel(16).0, mpsc::unbounded_channel().0).unwrap();
    assert_eq!(recovered.state_root(), replayed.state_root());
}
