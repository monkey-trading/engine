use alloy_primitives::B256;

use super::*;
use crate::merkle::keccak;

#[test]
fn a_discount_cuts_a_positive_markup_rounded_up_and_leaves_a_negative_one() {
    let bps = Fixed::from_bps;
    assert_eq!(discounted_markup(bps(250), 1_000), bps(225));
    assert_eq!(discounted_markup(bps(250), 2_000), bps(200));
    assert_eq!(discounted_markup(bps(250), 0), bps(250));
    assert_eq!(discounted_markup(Fixed::ZERO, 2_000), Fixed::ZERO);
    // A markup the discount cannot cut exactly rounds up, one unit above the exact cut.
    assert_eq!(discounted_markup(Fixed::from_raw(7), 1_000), Fixed::from_raw(7));
    assert_eq!(discounted_markup(Fixed::from_raw(15), 1_000), Fixed::from_raw(14));
    assert_eq!(discounted_markup(Fixed::from_raw(11), 2_000), Fixed::from_raw(9));
    assert_eq!(discounted_markup(bps(-50), 2_000), bps(-50));
    assert_eq!(discounted_markup(Fixed::from_raw(-7), 1_000), Fixed::from_raw(-7));
}

#[test]
fn every_charged_accounts_proof_verifies_at_every_tree_size_and_odd_carry() {
    for accounts in 1..=17u8 {
        let rows = rows(accounts);
        let leaves = charges_of(&rows).iter().map(|(account, charges)| leaf_hash(*account, charges)).collect();
        let root = root_of(leaves);
        for seed in 1..=accounts {
            let proof = proof(&rows, account(seed)).expect("a charged account has a proof");
            assert_eq!((proof.index, proof.count), (u64::from(seed - 1), u64::from(accounts)));
            assert!(proof.verify(root), "{accounts} accounts, account {seed}");
            // The path is no longer than the tree is tall, and shorter where the node carries up.
            assert!(proof.siblings.len() <= (usize::from(accounts)).next_power_of_two().trailing_zeros() as usize);
        }
        assert_eq!(proof(&rows, account(accounts + 1)), None, "an account charged nothing has no proof");
    }
}

#[test]
fn a_proof_fails_for_any_changed_charge_account_place_or_path() {
    let rows = rows(5);
    let root = root_of(charges_of(&rows).iter().map(|(account, charges)| leaf_hash(*account, charges)).collect());
    let good = proof(&rows, account(3)).expect("charged");
    assert!(good.verify(root));
    let changed: [fn(&mut FinancingProof); 8] = [
        |p| p.charges[0].amount += Fixed::from_raw(1),
        |p| p.charges[0].market += 1,
        |p| p.charges.push(Charge { market: 99, amount: Fixed::ONE }),
        |p| p.charges.clear(),
        |p| p.account = account(4),
        |p| p.index += 1,
        |p| p.count = p.index,
        |p| _ = p.siblings.pop(),
    ];
    for (case, change) in changed.into_iter().enumerate() {
        let mut proof = good.clone();
        change(&mut proof);
        assert!(!proof.verify(root), "change {case} still verifies");
    }
    let mut extra = good.clone();
    extra.siblings.push(B256::ZERO);
    assert!(!extra.verify(root), "a sibling past the root");
    assert!(!good.verify(B256::ZERO), "another root");
}

#[test]
fn a_leaf_commits_to_its_charge_count_and_order_and_differs_from_a_node() {
    let a = account(1);
    let one = Charge { market: 1, amount: Fixed::ONE };
    let two = Charge { market: 2, amount: -Fixed::ONE };
    assert_ne!(leaf_hash(a, &[one, two]), leaf_hash(a, &[two, one]));
    assert_ne!(leaf_hash(a, &[one]), leaf_hash(a, &[one, one]));
    assert_ne!(leaf_hash(a, &[one]), leaf_hash(account(2), &[one]));
    let leaf = leaf_hash(a, &[one]);
    assert_ne!(node_hash(leaf, leaf), keccak(&[leaf.as_slice(), leaf.as_slice()]), "a node is tagged");
    assert_eq!(root_of(vec![leaf]), leaf, "one leaf is its own root");
    assert_eq!(root_of(Vec::new()), B256::ZERO);
}

// ── Helpers ──

/// The account from `seed`.
fn account(seed: u8) -> AccountId {
    AccountId([seed; 20])
}

/// `accounts` accounts, the `n`th holding `n` positions of mixed sign, as a rollover derives them.
fn rows(accounts: u8) -> Vec<FinancingRow> {
    (1..=accounts)
        .flat_map(|seed| {
            (0..u32::from(seed)).map(move |market| FinancingRow {
                account: account(seed),
                market,
                amount: Fixed::from_raw(i128::from(seed) * 1_000 - i128::from(market) * 7),
            })
        })
        .collect()
}
