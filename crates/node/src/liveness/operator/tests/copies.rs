//! Two copies of one key at a start: a predecessor reopened from its copy beside the holder it handed
//! the keys to, which sequenced on, never passes the start check once a member answers.

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
