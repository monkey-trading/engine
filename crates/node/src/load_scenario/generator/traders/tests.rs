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
