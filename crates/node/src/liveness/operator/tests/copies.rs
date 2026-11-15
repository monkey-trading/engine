//! Two copies of one key at a start: a predecessor reopened from its copy beside the holder it handed
//! the keys to, which sequenced on, never passes the start check once a member answers, and signs no
//! head however another member confirms its tip.

use super::*;

/// A predecessor restarted from a reopened copy while the holder it handed the keys to keeps
/// sequencing past its final checkpoint: a member following the holder holds, and has seen, seqs this
/// copy lacks, so the start check refuses and pages the range, and nothing is sequenced or signed,
/// whether the holder's commands carry the handed-over key or a key promoted past it.
#[tokio::test(start_paused = true)]
async fn a_reopened_predecessor_beside_its_successor_never_passes_the_start_check() {
    let own = AccountId(EnclaveKey::test_key().address());
    for (case, sealer) in [("handed over", own), ("promoted past it", AccountId([0x77; 20]))] {
        let (_guard, mut alerts) = capture_alerts();
        // The copy ends at the handover's final checkpoint at seq 10; the successor sequenced on to 14.
        let rig = Rig::spawn(10);
        let ahead = Head { seq: 14, link: link(14) };
        let status = Status { epoch: 1, durable: Some(ahead), sealer, seen: Some(ahead) };
        rig.members.0.lock().unwrap().get_mut("member:1").unwrap().2 = Some(status);
        settle().await;
        settle().await;
        assert!(!rig.gate.start_checked() && !rig.gate.sequences(), "{case}: the start check refused");
        assert_eq!(*rig.signing.borrow(), Signing::Nothing, "{case}: no head is signed");
        let raised: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
        let refused: Vec<_> = raised.iter().filter(|alert| alert["alert"] == "liveness_start_refused").collect();
        assert!(refused.len() == 1 && refused[0]["message"].contains("11..=14"), "{case}: {raised:?}");
        rig.task.abort();
    }
}

/// A start check one member refuses signs no head, though another member's copy confirms this log's
/// tip: the node stays stopped, so heads stop and a promotion can pass it.
#[tokio::test(start_paused = true)]
async fn a_refused_start_check_signs_no_head_whatever_another_member_confirms() {
    let rig = Rig::spawn(10);
    let second = Arc::new(EnclaveKey::generate());
    rig.chain.set(|chain| {
        chain.members.push((Address::repeat_byte(0xc4), "member:2".into(), Address::from(second.address())));
    });
    let confirming =
        Status { epoch: 1, durable: Some(Head { seq: 10, link: link(10) }), sealer: AccountId([0; 20]), seen: None };
    rig.members.0.lock().unwrap().insert("member:2".into(), (AccountId([0xc4; 20]), second, Some(confirming)));
    let ahead = Head { seq: 14, link: link(14) };
    let status = Status { epoch: 1, durable: Some(ahead), sealer: AccountId([0; 20]), seen: Some(ahead) };
    rig.members.0.lock().unwrap().get_mut("member:1").unwrap().2 = Some(status);
    settle().await;
    settle().await;
    assert!(!rig.gate.start_checked(), "the member holding more refused the start");
    assert_eq!(*rig.signing.borrow(), Signing::Nothing, "a head was signed at the confirmed tip");
    // The member past the tip goes quiet: the other's confirmation alone passes the check, and heads go.
    rig.members.0.lock().unwrap().get_mut("member:1").unwrap().2 = None;
    settle().await;
    settle().await;
    assert!(rig.gate.start_checked() && matches!(*rig.signing.borrow(), Signing::Confirmed(_)));
    rig.task.abort();
}
