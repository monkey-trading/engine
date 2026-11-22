use super::*;

#[test]
fn a_cancel_or_close_all_whose_target_is_gone_goes_out_as_a_market_order() {
    let mut trader = Trader::new(1);
    let units = BTreeMap::from([(2, Fixed::from_int(50))]);
    let job =
        |kind| Job { trader: 0, kind, market: 2, side: Side::Buy, due: Instant::now(), dispatched: Instant::now() };
    for kind in [Kind::Cancel, Kind::CancelAll, Kind::CloseAll] {
        let mut gone = job(kind);
        trader.settle(&mut gone);
        assert_eq!(gone.kind, Kind::Market, "{kind:?} with nothing to act on");
        assert_eq!(trader.request(&gone, &units).0, "/v1/orders");
    }
    {
        let mut holding = trader.holding.lock().unwrap();
        holding.resting.push(7);
        holding.positions.insert(2, Side::Sell);
    }
    for (kind, path) in [
        (Kind::Cancel, "/v1/orders/cancel"),
        (Kind::CancelAll, "/v1/orders/cancel-all"),
        (Kind::CloseAll, "/v1/positions/close-all"),
    ] {
        let mut held = job(kind);
        trader.settle(&mut held);
        assert_eq!(held.kind, kind, "{kind:?} with a target");
        assert_eq!(trader.request(&held, &units).0, path);
    }
}

#[test]
fn a_cancel_the_node_rejects_as_unknown_drops_the_order_it_asked_for_and_another_rejection_keeps_it() {
    let units = BTreeMap::from([(2, Fixed::from_int(50))]);
    let cancel = Job {
        trader: 0,
        kind: Kind::Cancel,
        market: 2,
        side: Side::Buy,
        due: Instant::now(),
        dispatched: Instant::now(),
    };
    let rejection = |trader: &Trader, kind: &str| json!([{ "type": "order_rejected", "account": trader.account, "client_id": 0, "reason": { "kind": kind } }]);
    // A reply lost to a restart left order 9 listed, which the node no longer has.
    let mut trader = Trader::new(1);
    trader.holding.lock().unwrap().resting.extend([7, 9]);
    assert_eq!(trader.request(&cancel, &units).0, "/v1/orders/cancel");
    let reply = rejection(&trader, "unknown_order");
    assert_eq!(trader.take(200, &reply).1.as_deref(), Some("unknown_order"));
    assert_eq!(trader.holding.lock().unwrap().resting, [7], "the order the cancel named is gone");
    // Any other rejection leaves the order resting: the node still has it.
    let mut trader = Trader::new(1);
    trader.holding.lock().unwrap().resting.push(9);
    trader.request(&cancel, &units);
    let reply = rejection(&trader, "market_closed");
    trader.take(200, &reply);
    assert_eq!(trader.holding.lock().unwrap().resting, [9]);
    // A rejection of a request that was no cancel names no order.
    let mut trader = Trader::new(1);
    trader.holding.lock().unwrap().resting.push(9);
    trader.request(&cancel, &units);
    let market = Job { kind: Kind::Market, ..cancel };
    trader.request(&market, &units);
    let reply = rejection(&trader, "unknown_order");
    trader.take(200, &reply);
    assert_eq!(trader.holding.lock().unwrap().resting, [9], "no cancel was asked for");
}
