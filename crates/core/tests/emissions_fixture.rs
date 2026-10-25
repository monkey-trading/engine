//! The entitlement proofs the vault's tests claim with (`contracts/test/Emissions.t.sol`):
//! `fixtures/emissions.json` must be exactly what the engine proves for this session, and each proof
//! must verify against its root.

mod common;

use common::Harness;
use common::emissions::{funded, next_period, round_trip};
use engine_core::merkle::EntitlementProof;

const FIXTURE: &str = include_str!("fixtures/emissions.json");

#[test]
fn the_vault_test_vectors_are_the_engines_entitlement_proofs() {
    let mut h = Harness::emitting(1_000_000);
    let [a, b] = funded(&mut h);
    round_trip(&mut h, &[(a, "1"), (b, "3")], "100000", "99900");
    next_period(&mut h);
    h.engine.state_root();
    let (first, other) = (h.engine.entitlement_proof(a).unwrap(), h.engine.entitlement_proof(b).unwrap());
    round_trip(&mut h, &[(a, "1")], "100000", "99900");
    next_period(&mut h);
    h.engine.state_root();
    let later = h.engine.entitlement_proof(a).unwrap();

    let computed = serde_json::json!({ "first": first, "other": other, "later": later });
    let pretty = serde_json::to_string_pretty(&computed).unwrap();
    assert_eq!(serde_json::from_str::<serde_json::Value>(FIXTURE).unwrap(), computed, "expected:\n{pretty}");
    for proof in [&first, &other, &later] {
        assert!(proof.verify(proof.root));
    }
    // The vault's tests rely on these relations between the vectors.
    assert_eq!((first.account, later.account), (a, a));
    assert!(later.entitled > first.entitled && first.root == other.root && later.root != first.root);
    assert!(!EntitlementProof { root: later.root, ..first }.verify(later.root));
}
