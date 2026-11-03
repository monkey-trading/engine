//! The close-outs `GET /v1/accounts/{address}` serves with each position.

use axum::http::StatusCode;
use engine_core::Fixed;
use engine_core::command::{OrderKind, OrderRequest};
use engine_core::types::Side;

use super::proof_tests::{Node, account};

#[tokio::test]
async fn the_account_read_names_a_close_out_price_below_a_longs_mark_and_none_for_a_flat_account() {
    let node = Node::start("account-closeouts").await;
    let trader = account(7);
    node.deposit(trader, "100000").await;
    let (status, flat) = node.get(&format!("/v1/accounts/{trader}")).await;
    assert_eq!((status, flat["closeouts"].as_array().map(Vec::len)), (StatusCode::OK, Some(0)));
    node.price_btc("100000").await;
    let order = OrderRequest {
        account: trader,
        client_id: 1,
        market: 1,
        side: Side::Buy,
        qty: "5".parse().unwrap(),
        kind: OrderKind::Market { quoted: "100000".parse().unwrap(), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    };
    assert_eq!(node.post("/v1/orders", serde_json::to_value(order).unwrap()).await.0, StatusCode::OK);
    let (_, view) = node.get(&format!("/v1/accounts/{trader}")).await;
    let closeouts = view["closeouts"].as_array().unwrap();
    assert_eq!(closeouts.len(), 1, "{view}");
    assert_eq!((closeouts[0]["market"].as_u64(), closeouts[0]["reach"].as_str()), (Some(1), Some("at")));
    let price: Fixed = closeouts[0]["price"].as_str().unwrap().parse().unwrap();
    assert!(
        price > Fixed::from_int(75_000) && price < Fixed::from_int(90_000),
        "a long of 5 on 100000 closes out about 20% down: {price}"
    );
}
