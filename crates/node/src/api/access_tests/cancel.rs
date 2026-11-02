//! A client can cancel its resting order while geo access is excluded, without a screening call.

use super::*;

#[tokio::test]
async fn a_client_can_cancel_its_resting_order_while_geo_access_is_excluded() {
    let node = Node::start("access-cancel", true).await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    node.onboard(&client).await;
    let mut request: OrderRequest = serde_json::from_value(order(&client, 1)).unwrap();
    request.kind = OrderKind::Limit { price: Fixed::from_int(90000) };
    let hash = eip712::order_hash(&eip712::domain(31_337, VAULT), &request, 1, NOW + 60_000);
    request.auth.as_mut().unwrap().signature = eip712::sign(&client, &hash);
    let accepted = node.post("/v1/orders", json!(request)).await.1;
    assert_eq!(accepted[0]["type"], "order_accepted");
    let order_id = accepted[0]["order_id"].as_u64().unwrap();
    node.source.lock().unwrap().jurisdiction = Jurisdiction::UnitedStates;
    let queries = node.source.lock().unwrap().requests.len();
    let hash = eip712::cancel_hash(&eip712::domain(31_337, VAULT), account, order_id, 2, NOW + 60_000);
    let request = engine_core::command::CancelRequest {
        account,
        order_id,
        auth: Some(RequestAuth { nonce: 2, expires_at: NOW + 60_000, signature: eip712::sign(&client, &hash) }),
    };
    let (status, events) = node.post("/v1/orders/cancel", json!(request)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(events[0]["type"], "order_cancelled");
    assert_eq!(events[0]["reason"], "client");
    assert_eq!(node.source.lock().unwrap().requests.len(), queries);
    assert!(node.state.query(move |engine| engine.accounts()[&account].orders.is_empty()).await.unwrap());
}
