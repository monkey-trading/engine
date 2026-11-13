//! The account leaves the vault's exit hashes, across `V54`'s activation: an account scored under
//! `V11` proves after it with the encoding of an unscored account, and one with unfinalised
//! principal with the suffix that carries it.

use alloy_primitives::{B256, keccak256};
use alloy_sol_types::SolValue;
use engine_core::Fixed;
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::AdminCommand;
use engine_core::merkle::AccountLeaf;
use engine_core::protocol::ProtocolVersion;

use crate::common::Harness;
use crate::common::close_spread::install_expect;
use crate::custody::custody;
use crate::support::trading;

/// Installs a ceiling for every listed market, as the `V40` activation wants, then activates `V54`.
fn into_v54(h: &mut Harness) {
    let ceilings = h
        .engine
        .markets()
        .keys()
        .map(|market| CloseSpreadCeiling { market: *market, max_close_spread_bps: 50 })
        .collect();
    install_expect(h, ceilings);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V54 });
}

/// `abi.encode(AccountLeaf)` of the leaf's balance, nonce, score in whole bps and position count.
fn struct_encoding(leaf: &AccountLeaf) -> Vec<u8> {
    (leaf.balance.raw(), leaf.withdrawal_nonce, leaf.toxicity_bps, leaf.open_positions).abi_encode()
}

#[test]
fn from_v54_an_account_scored_before_proves_with_the_leaf_of_an_unscored_one() {
    let (mut h, trader, _) = trading();
    let scored = &h.engine.accounts()[&trader];
    assert!(!scored.toxicity_history.is_zero(), "the fixture's fills scored the account under V11");
    into_v54(&mut h);
    let root = B256::from(h.engine.state_root());
    let proof = h.engine.exit_proof(trader).unwrap().account.unwrap();
    assert!(proof.verify(root));
    let leaf = &proof.leaf;
    assert_eq!((leaf.toxicity_bps, leaf.toxicity_fraction, leaf.toxicity_history), (0, Fixed::ZERO, Fixed::ZERO));
    assert!(leaf.unfinalised_deposits.is_zero());
    assert_eq!(leaf.value_hash(), keccak256(struct_encoding(leaf)));
    assert_eq!(proof.equity(), Some(h.equity_and_margin(trader).0));
}

#[test]
fn from_v54_an_account_with_unfinalised_principal_proves_with_its_suffix() {
    let (mut h, [client, ..], _) = custody();
    into_v54(&mut h);
    let root = B256::from(h.engine.state_root());
    let proof = h.engine.exit_proof(client).unwrap().account.unwrap();
    assert!(proof.verify(root));
    let leaf = &proof.leaf;
    assert!(leaf.unfinalised_deposits.is_positive(), "{leaf:?}");
    let suffix = (Fixed::ZERO.raw(), Fixed::ZERO.raw(), leaf.unfinalised_deposits.raw()).abi_encode();
    assert_eq!(leaf.value_hash(), keccak256([struct_encoding(leaf), suffix].concat()));
}
