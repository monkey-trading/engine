//! Who may deliver secrets to a running node: the key the log records, as this route checks it.

use super::*;

use crate::secrets::Authority;

#[tokio::test]
async fn a_running_node_takes_deliveries_from_the_key_in_effect_and_no_other() {
    let mut node = Node::start_on("delivery-authority", true).await;
    let old = node.state.operator.address;
    let next = SigningKey::from_slice(&[0x55; 32]).unwrap();
    let address = engine_core::eip712::address(next.verifying_key());
    let stranger = AccountId([7; 20]);
    let authority = |clock| Authority::Log { tx: node.state.tx.clone(), configured: old, clock };

    let before = authority(node.state.clock.clone());
    assert!(before.admits(old).await.unwrap(), "the recorded key");
    assert!(!before.admits(address).await.unwrap(), "a key no rotation names");
    assert!(!before.admits(stranger).await.unwrap(), "a stranger");

    let rotation = node.sign(0, AdminCommand::RotateOperator { address }, NOW + 60_000);
    let (status, _) = node.post("/v1/admin", serde_json::to_value(&rotation).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(before.admits(old).await.unwrap(), "the recorded key during the notice");
    assert!(!before.admits(address).await.unwrap(), "the new key before its notice has run");

    // Once the notice has run the new key is due; the old one still is, until the engine applies a
    // command under the new key, as the admin route takes both.
    node.stamps.advance(Duration::from_millis(after_the_notice() as u64 - NOW as u64));
    let sample = sample_around(node.stamps.as_ref());
    node.state.tx.send(crate::sequencer::Request::TimeSample(sample)).await.unwrap();
    let clock = std::sync::Arc::new(crate::clock::test_clock::TestClock::at(after_the_notice()));
    node.state.clock = clock.clone();
    let due = authority(clock);
    assert!(due.admits(address).await.unwrap(), "the new key once it is due");
    assert!(!due.admits(stranger).await.unwrap(), "a stranger once the rotation is due");

    let expires_at = after_the_notice() + 60_000;
    let handover = sign(&next, &node.state.operator, reduce_only(Some(1)), 0, expires_at);
    let (status, _) = node.post("/v1/admin", serde_json::to_value(&handover).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(due.admits(address).await.unwrap(), "the recorded key after the handover");
    assert!(!due.admits(old).await.unwrap(), "the key the log has moved on from");
}

// During a promotion's notice the outgoing key still signs admin commands, but the node the rotation
// names takes deliveries only from its own operator, its configured key.
#[tokio::test]
async fn a_named_successor_takes_deliveries_from_its_own_key_and_not_the_outgoing_one() {
    let node = Node::start_on("delivery-successor", true).await;
    let outgoing = node.state.operator.address;
    let own = engine_core::eip712::address(SigningKey::from_slice(&[0x55; 32]).unwrap().verifying_key());
    let successor = Authority::Log { tx: node.state.tx.clone(), configured: own, clock: node.state.clock.clone() };
    assert!(!successor.admits(own).await.unwrap(), "no rotation names this node yet");

    let rotation = node.sign(0, AdminCommand::RotateOperator { address: own }, NOW + 60_000);
    let (status, _) = node.post("/v1/admin", serde_json::to_value(&rotation).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(successor.admits(own).await.unwrap(), "the named successor's own key, before its notice has run");
    assert!(!successor.admits(outgoing).await.unwrap(), "the outgoing key on its successor");
    assert!(!successor.admits(AccountId([7; 20])).await.unwrap(), "a stranger");
}
