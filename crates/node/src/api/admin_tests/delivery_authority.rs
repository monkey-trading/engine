//! Who may deliver secrets to a running node: the key the log records, as this route checks it.

use super::*;

use crate::secrets::Authority;
use engine_core::election::RegistryElection;

#[tokio::test]
async fn a_running_node_takes_deliveries_from_the_key_in_effect_and_no_other() {
    let mut node = Node::start_on("delivery-authority", true).await;
    let old = node.state.operator.address;
    let next = SigningKey::from_slice(&[0x55; 32]).unwrap();
    let address = engine_core::eip712::address(next.verifying_key());
    let stranger = AccountId([7; 20]);
    let authority =
        |clock: std::sync::Arc<dyn crate::clock::Clock>, tx: &tokio::sync::mpsc::Sender<_>| Authority::Log {
            tx: tx.downgrade(),
            configured: old,
            trusted: std::sync::Arc::new(crate::test_support::trusted(clock)),
        };

    let before = authority(node.stamps.clone(), &node.state.tx);
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
    let due = authority(clock, &node.state.tx);
    assert!(due.admits(address).await.unwrap(), "the new key once it is due");
    assert!(!due.admits(stranger).await.unwrap(), "a stranger once the rotation is due");

    let expires_at = after_the_notice() + 60_000;
    let handover = sign(&next, &node.state.operator, reduce_only(Some(1)), 0, expires_at);
    let (status, _) = node.post("/v1/admin", serde_json::to_value(&handover).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(due.admits(address).await.unwrap(), "the recorded key after the handover");
    assert!(!due.admits(old).await.unwrap(), "the key the log has moved on from");
}

// Rotation turning on records no operator change by itself: until an election is recorded the key
// the log records keeps delivering, and the configured key the log rotated away from does not.
#[tokio::test]
async fn rotation_on_without_an_election_keeps_the_log_key_in_effect() {
    let mut node = Node::staking("delivery-rotation-no-election").await;
    let old = node.state.operator.address;
    let next = SigningKey::from_slice(&[0x55; 32]).unwrap();
    let rotated = engine_core::eip712::address(next.verifying_key());

    // With rotation off the operator rotates to `rotated`, and the log records it once the
    // notice has run and a command applies under the new key.
    let rotation = node.sign(0, AdminCommand::RotateOperator { address: rotated }, NOW + 60_000);
    let (status, _) = node.post("/v1/admin", serde_json::to_value(&rotation).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    node.stamps.advance(Duration::from_millis(after_the_notice() as u64 - NOW as u64));
    let sample = sample_around(node.stamps.as_ref());
    node.state.tx.send(crate::sequencer::Request::TimeSample(sample)).await.unwrap();
    node.state.clock = std::sync::Arc::new(crate::clock::test_clock::TestClock::at(after_the_notice()));
    let handover = sign(&next, &node.state.operator, reduce_only(Some(1)), 0, after_the_notice() + 60_000);
    let (status, _) = node.post("/v1/admin", serde_json::to_value(&handover).unwrap()).await;
    assert_eq!(status, StatusCode::OK);

    // The timelock turns rotation on, with no election recorded.
    let on = Command::SwitchRotation { enabled: true, election_cancelled: false, block: 1, chain_ref: "0xon:0".into() };
    assert_eq!(
        node.state.submit(on).await.unwrap(),
        [Event::RotationSwitched { enabled: true, election_cancelled: false }]
    );

    let authority = Authority::Log {
        tx: node.state.tx.downgrade(),
        configured: old,
        trusted: std::sync::Arc::new(crate::test_support::trusted(node.state.clock.clone())),
    };
    assert!(authority.admits(rotated).await.unwrap(), "the key the log records");
    assert!(!authority.admits(old).await.unwrap(), "the configured key the log rotated away from");
    assert!(!authority.admits(AccountId([7; 20])).await.unwrap(), "a stranger");

    // A recorded election narrows deliveries to the configured key, but one the registry
    // cancelled before it took effect does not: with no election in effect the log's key stays.
    let elected = engine_core::eip712::address(SigningKey::from_slice(&[0x66; 32]).unwrap().verifying_key());
    let election = Command::ElectOperator(RegistryElection {
        epoch: 1,
        candidate: "0x00000000000000000000000000000000000000c1".parse().unwrap(),
        admin_key: elected,
        effective_at: NOW + 3_600_000,
        block: 2,
        chain_ref: "0xelect:0".into(),
    });
    let events = node.state.submit(election).await.unwrap();
    assert!(matches!(events.last(), Some(Event::OperatorElected { .. })), "{events:?}");
    assert!(!authority.admits(elected).await.unwrap(), "the elected key before its time");
    assert!(!authority.admits(rotated).await.unwrap(), "the log's key once an election narrows deliveries");
    assert!(authority.admits(old).await.unwrap(), "the configured key once an election narrows deliveries");

    let off =
        Command::SwitchRotation { enabled: false, election_cancelled: true, block: 3, chain_ref: "0xoff:0".into() };
    assert_eq!(
        node.state.submit(off).await.unwrap(),
        [Event::RotationSwitched { enabled: false, election_cancelled: true }]
    );
    let on =
        Command::SwitchRotation { enabled: true, election_cancelled: false, block: 4, chain_ref: "0xon2:0".into() };
    assert_eq!(
        node.state.submit(on).await.unwrap(),
        [Event::RotationSwitched { enabled: true, election_cancelled: false }]
    );
    assert!(authority.admits(rotated).await.unwrap(), "the recorded key with the election cancelled");
    assert!(!authority.admits(elected).await.unwrap(), "the cancelled election's key");
    assert!(!authority.admits(old).await.unwrap(), "the configured key the log rotated away from");
}

// During a promotion's notice the outgoing key still signs admin commands, but the node the rotation
// names takes deliveries only from its own operator, its configured key.
#[tokio::test]
async fn a_named_successor_takes_deliveries_from_its_own_key_and_not_the_outgoing_one() {
    let node = Node::start_on("delivery-successor", true).await;
    let outgoing = node.state.operator.address;
    let own = engine_core::eip712::address(SigningKey::from_slice(&[0x55; 32]).unwrap().verifying_key());
    let successor = Authority::Log {
        tx: node.state.tx.downgrade(),
        configured: own,
        trusted: std::sync::Arc::new(crate::test_support::trusted(node.stamps.clone())),
    };
    assert!(!successor.admits(own).await.unwrap(), "no rotation names this node yet");

    let rotation = node.sign(0, AdminCommand::RotateOperator { address: own }, NOW + 60_000);
    let (status, _) = node.post("/v1/admin", serde_json::to_value(&rotation).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(successor.admits(own).await.unwrap(), "the named successor's own key, before its notice has run");
    assert!(!successor.admits(outgoing).await.unwrap(), "the outgoing key on its successor");
    assert!(!successor.admits(AccountId([7; 20])).await.unwrap(), "a stranger");
}

// A host that moves the guest's clock past the notice does not bring the new key in: with nothing to
// bound the clock the rotation is not due, and with a sample that disagrees with it neither.
#[tokio::test]
async fn an_unbounded_clock_does_not_bring_a_pending_key_in_early() {
    let node = Node::start_on("delivery-untrusted-clock", true).await;
    let next = engine_core::eip712::address(SigningKey::from_slice(&[0x55; 32]).unwrap().verifying_key());
    let rotation = node.sign(0, AdminCommand::RotateOperator { address: next }, NOW + 60_000);
    let (status, _) = node.post("/v1/admin", serde_json::to_value(&rotation).unwrap()).await;
    assert_eq!(status, StatusCode::OK);

    let moved: std::sync::Arc<dyn crate::clock::Clock> =
        std::sync::Arc::new(crate::clock::test_clock::TestClock::at(after_the_notice()));
    let no_sample = crate::trusted_time::TrustedClock::new(
        moved.clone(),
        tokio::sync::watch::channel(None).1,
        dev_config().time.max_drift,
    );
    let honest_sample = sample_around(node.stamps.as_ref());
    let contradicted = crate::trusted_time::TrustedClock::new(
        moved,
        tokio::sync::watch::channel(Some(honest_sample)).1,
        dev_config().time.max_drift,
    );
    for trusted in [no_sample, contradicted] {
        let authority = Authority::Log {
            tx: node.state.tx.downgrade(),
            configured: node.state.operator.address,
            trusted: std::sync::Arc::new(trusted),
        };
        assert!(!authority.admits(next).await.unwrap(), "the pending key on the host's word");
        assert!(authority.admits(node.state.operator.address).await.unwrap(), "the recorded key stays");
    }
}

// Under elections each operator's secrets are its own. Once the log records the elected key, that key
// must not reach into the incumbent's enclave, and the incumbent's own still does.
#[tokio::test]
async fn an_elected_key_takes_no_deliveries_on_the_incumbents_node() {
    let mut node = Node::staking("delivery-elected").await;
    let incumbent = node.state.operator.address;
    let next = SigningKey::from_slice(&[0x55; 32]).unwrap();
    let elected = engine_core::eip712::address(next.verifying_key());
    let effective_at = node.elect(elected).await;
    node.pass_the_notice(effective_at).await;
    let authority = Authority::Log {
        tx: node.state.tx.downgrade(),
        configured: incumbent,
        trusted: std::sync::Arc::new(crate::test_support::trusted(node.state.clock.clone())),
    };
    assert!(!authority.admits(elected).await.unwrap(), "the elected key once it is due");
    assert!(authority.admits(incumbent).await.unwrap(), "this node's own key once the election is due");

    let handover = sign(&next, &node.state.operator, reduce_only(Some(1)), 0, after_the_notice() + 60_000);
    let (status, _) = node.post("/v1/admin", serde_json::to_value(&handover).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!authority.admits(elected).await.unwrap(), "the key the log now records");
    assert!(authority.admits(incumbent).await.unwrap(), "this node's own key after the handover");
}
