//! An account proof shows each position's levels or that it holds none, so a proof cannot leave a
//! position's levels out.

use alloy_primitives::B256;
use engine_core::merkle::{AccountProof, LevelsProof, NoLevelsProof, levels_key};

use crate::common::BTC;
use crate::common::close_spread::trade;
use crate::common::events::only_fill;
use crate::common::levels::{funded, levels, open_with, set};
use engine_core::types::Side;

fn proof_of(h: &mut crate::common::Harness, a: &crate::common::Client) -> (AccountProof, B256) {
    let root = B256::from(h.engine.state_root());
    (h.engine.account_proof(a.account).expect("the account has a leaf"), root)
}

#[test]
fn a_proof_shows_a_positions_levels_and_cannot_leave_them_out() {
    let (mut h, a, _) = funded();
    open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(Some("102000"), Some("98000")));
    let (proof, root) = proof_of(&mut h, &a);
    assert!(proof.verify(root));
    assert_eq!((proof.levels.len(), proof.no_levels.len()), (1, 0));
    assert_eq!(proof.levels[0].levels, levels(Some("102000"), Some("98000")));

    // Left out, or replaced by a proof that the place is empty, the proof fails.
    let mut left_out = proof.clone();
    left_out.levels.clear();
    assert!(!left_out.verify(root));
    let mut hidden = proof.clone();
    let LevelsProof { market, .. } = hidden.levels.remove(0);
    for other in [None, Some((B256::repeat_byte(1), B256::repeat_byte(2)))] {
        hidden.no_levels = vec![NoLevelsProof { market, other, siblings: proof.levels[0].siblings.clone() }];
        assert!(!hidden.verify(root));
    }
    // Levels other than the ones held fail too.
    let mut altered = proof.clone();
    altered.levels[0].levels = levels(Some("103000"), Some("98000"));
    assert!(!altered.verify(root));
    // The proof survives its encoding.
    let encoded = serde_json::to_string(&proof).unwrap();
    assert_eq!(serde_json::from_str::<AccountProof>(&encoded).unwrap(), proof);
}

#[test]
fn a_proof_shows_a_position_with_no_levels_and_cannot_claim_levels_it_does_not_hold() {
    let (mut h, a, _) = funded();
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "1", "100000"));
    let (proof, root) = proof_of(&mut h, &a);
    assert!(proof.verify(root));
    assert_eq!((proof.levels.len(), proof.no_levels.len()), (0, 1));
    let mut left_out = proof.clone();
    left_out.no_levels.clear();
    assert!(!left_out.verify(root));
    // Levels removed by request leave the same absence to prove.
    set(&mut h, &a, BTC, levels(None, Some("98000")));
    assert_eq!(proof_of(&mut h, &a).0.levels.len(), 1);
    set(&mut h, &a, BTC, levels(None, None));
    let (removed, root) = proof_of(&mut h, &a);
    assert!(removed.verify(root) && removed.levels.is_empty() && removed.no_levels.len() == 1);
    // A proof of levels the position does not hold fails, whatever siblings it carries.
    let mut forged = removed.clone();
    let market = forged.no_levels.remove(0).market;
    forged.levels = vec![LevelsProof {
        market,
        levels: levels(None, Some("98000")),
        siblings: removed.no_levels[0].siblings.clone(),
    }];
    assert!(!forged.verify(root));
    assert_ne!(levels_key(a.account, market), B256::ZERO);
}
