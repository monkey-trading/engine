//! A declaration is screened like an order: a refusal is public and uses the nonce, a source failure
//! raises the source alert, and the account's deposit waits pending until a later declaration with
//! verified, allowed and clear evidence.

use super::*;
use crate::trusted_time::tests::logs::Logs;

#[tokio::test]
async fn a_refused_declaration_is_public_and_its_deposit_waits_pending_for_an_allowed_one() {
    let (logs, _guard) = Logs::capture();
    let node = Node::start("declaration-screening", true).await;
    let client = key(1);
    let account = eip712::address(client.verifying_key());
    let pending = node.post("/v1/dev/deposit", json!({"account":account,"amount":"100000"})).await.1;
    assert_eq!(pending[0]["reason"], "eligibility_required");
    let reference = pending[0]["chain_ref"].as_str().unwrap().to_owned();
    let cases = [
        ("united_states", Jurisdiction::UnitedStates, false, false),
        ("sanctioned_jurisdiction", Jurisdiction::Sanctioned, false, false),
        ("jurisdiction_unknown", Jurisdiction::Unknown, false, false),
        ("account_sanctioned", Jurisdiction::Allowed, true, false),
        ("source_invalid", Jurisdiction::Allowed, false, true),
    ];
    for (nonce, (expected, jurisdiction, sanctioned, forged)) in (0..).zip(cases) {
        {
            let mut source = node.source.lock().unwrap();
            source.jurisdiction = jurisdiction;
            source.forged = forged;
            if sanctioned {
                source.sanctioned.insert(account);
            } else {
                source.sanctioned.clear();
            }
        }
        let (status, events) = node.post("/v1/eligibility", declaration(&client, nonce)).await;
        assert_eq!(status, StatusCode::OK, "{events}");
        assert_eq!(
            events,
            json!([{"type":"eligibility_rejected","account":account,
            "reason":{"kind":"access","reason":expected}}])
        );
        let screened = node.source.lock().unwrap().requests.last().cloned().unwrap();
        assert_eq!(screened["account"], json!(account));
        assert_eq!(screened["peer"], "127.0.0.1", "the transport peer, never a forwarding header");
    }
    let state = move |engine: &Engine| {
        (engine.eligibility(account), engine.accounts().contains_key(&account), engine.next_request_nonce(account))
    };
    assert_eq!(node.state.query(state).await.unwrap(), (None, false, 5));
    assert_eq!(logs.count("alert=\"access_source_failed\""), 1, "only the forged evidence is a source failure");
    assert_eq!(node.get(&format!("/v1/access/pending/{reference}")).await["amount"], "100000");
    {
        let mut source = node.source.lock().unwrap();
        (source.jurisdiction, source.forged) = (Jurisdiction::Allowed, false);
        source.sanctioned.clear();
    }
    let (status, events) = node.post("/v1/eligibility", declaration(&client, 5)).await;
    assert_eq!((status, &events[0]["type"]), (StatusCode::OK, &json!("eligibility_attested")), "{events}");
    assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), 1, "a recorded declaration is verified");
    node.retry_pending(&reference).await;
    let replayed = replay(&node.entries()).unwrap().engine;
    assert_eq!(replayed.accounts()[&account].balance, Fixed::from_int(100000));
    let refusals = node
        .entries()
        .iter()
        .filter(|entry| {
            matches!(entry, LogEntry::Command(command)
        if command.events.as_ref().is_some_and(|events| events.iter().any(|event|
            matches!(event, Event::EligibilityRejected { .. }))))
        })
        .count();
    assert_eq!(refusals, cases.len(), "every refusal is in the public log");
}
