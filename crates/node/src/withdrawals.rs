//! Vault withdrawal authorizations, bound to a durable approval and its accepted state root.

use alloy::primitives::{Address, U256, keccak256};
use alloy::sol_types::{Eip712Domain, SolValue};
use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;
use serde::{Deserialize, Serialize};

use crate::log::CheckpointDomain;
use crate::sequencer::SignedWithdrawal;

/// A withdrawal authorization lasts exactly one day, in Ethereum timestamp seconds.
pub(crate) const LIFETIME_SECONDS: u64 = 86_400;
const TYPE: &str = "Withdrawal(address account,uint256 amount,uint64 nonce,uint64 approvedSeq,uint64 signerEpoch,uint64 issuedAt,uint64 expiresAt)";

/// Custody evidence accompanying an EIP-712 authorization. The proof uses root-to-leaf siblings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Authorization {
    pub signer: AccountId,
    pub signer_epoch: u64,
    pub issued_at: u64,
    pub expires_at: u64,
    pub checkpoint_seq: u64,
    pub checkpoint_root: B256,
    pub siblings: Vec<B256>,
}

/// Converts a positive whole-USDC-base-unit amount without truncation.
/// Error cases:
/// - the amount is nonpositive, out of bounds, or has sub-base-unit precision
pub(crate) fn token_units(amount: Fixed) -> anyhow::Result<U256> {
    ensure!(amount.is_valid_input() && amount.raw() % 1_000 == 0, "withdrawal amount is not whole USDC base units");
    Ok(U256::from(u128::try_from(amount.raw() / 1_000)?))
}

/// Computes the vault's EIP-712 digest using ABI encoding and the alloy domain implementation.
/// Error cases:
/// - the authorization, approval sequence, or exact token amount is absent or invalid
pub(crate) fn digest(withdrawal: &SignedWithdrawal) -> anyhow::Result<B256> {
    let auth = withdrawal.authorization.as_ref().context("withdrawal has no typed authorization")?;
    let approved_seq = withdrawal.approved_seq.context("typed withdrawal has no approval sequence")?;
    ensure!(
        auth.issued_at.checked_add(LIFETIME_SECONDS) == Some(auth.expires_at),
        "withdrawal expiry is not exactly 24 hours after issue"
    );
    ensure!(approved_seq <= auth.checkpoint_seq, "withdrawal checkpoint predates its approval");
    let domain = Eip712Domain::new(
        Some("EngineWithdrawal".into()),
        Some("1".into()),
        Some(U256::from(withdrawal.chain_id)),
        Some(Address::from(withdrawal.vault.0)),
        None,
    );
    let value = keccak256(
        (
            keccak256(TYPE),
            Address::from(withdrawal.account.0),
            token_units(withdrawal.amount)?,
            withdrawal.nonce,
            approved_seq,
            auth.signer_epoch,
            auth.issued_at,
            auth.expires_at,
        )
            .abi_encode(),
    );
    Ok(keccak256([&[0x19, 0x01][..], domain.hash_struct().as_slice(), value.as_slice()].concat()))
}

/// Checks an authorization's exact approval proof and independently trusted signer/deployment.
/// Error cases:
/// - malformed fields, an invalid inclusion proof, or an unauthorized signature
pub(crate) fn verify(withdrawal: &SignedWithdrawal, domain: CheckpointDomain, signer: AccountId) -> anyhow::Result<()> {
    let auth = withdrawal.authorization.as_ref().context("withdrawal has no typed authorization")?;
    ensure!(withdrawal.chain_id == domain.chain_id && withdrawal.vault == domain.vault, "withdrawal domain mismatch");
    ensure!(signer.0 != [0; 20] && auth.signer == signer, "unauthorized withdrawal signer");
    let signature: Signature = withdrawal.signature.parse().context("invalid withdrawal signature encoding")?;
    let hash = digest(withdrawal)?;
    ensure!(eip712::recover(&hash, &signature) == Some(signer), "invalid typed withdrawal signature");
    let key = keccak256([&[32][..], &withdrawal.account.0, &withdrawal.nonce.to_be_bytes()].concat());
    let value = keccak256(
        (
            Address::from(withdrawal.account.0),
            token_units(withdrawal.amount)?,
            withdrawal.nonce,
            withdrawal.approved_seq.context("withdrawal has no approval sequence")?,
        )
            .abi_encode(),
    );
    ensure!(
        engine_core::merkle::verify(auth.checkpoint_root, key, value, &auth.siblings),
        "withdrawal approval is absent from its checkpoint"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The same bytes are consumed by Foundry's vault tests.
    pub(crate) fn vector() -> (SignedWithdrawal, serde_json::Value) {
        let vector: serde_json::Value =
            serde_json::from_str(include_str!("../../../contracts/test/fixtures/withdrawal.json")).unwrap();
        let fields = &vector["withdrawal"];
        let number = |name: &str| fields[name].as_u64().unwrap();
        let withdrawal = SignedWithdrawal {
            account: fields["account"].as_str().unwrap().parse().unwrap(),
            amount: Fixed::from_raw(i128::from(number("amount")) * 1_000),
            nonce: number("nonce"),
            chain_id: vector["chainId"].as_u64().unwrap(),
            vault: vector["verifyingContract"].as_str().unwrap().parse().unwrap(),
            signature: vector["signature"].as_str().unwrap().to_owned(),
            approved_seq: Some(number("approvedSeq")),
            authorization: Some(Authorization {
                signer: vector["signer"].as_str().unwrap().parse().unwrap(),
                signer_epoch: number("signerEpoch"),
                issued_at: number("issuedAt"),
                expires_at: number("expiresAt"),
                checkpoint_seq: vector["checkpointSeq"].as_u64().unwrap(),
                checkpoint_root: vector["root"].as_str().unwrap().parse().unwrap(),
                siblings: vector["siblings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|hash| hash.as_str().unwrap().parse().unwrap())
                    .collect(),
            }),
        };
        (withdrawal, vector)
    }

    #[test]
    fn vault_vector_digest_signature_and_approval_proof_match_exactly() {
        let (withdrawal, vector) = vector();
        assert_eq!(digest(&withdrawal).unwrap(), vector["digest"].as_str().unwrap().parse::<B256>().unwrap());
        let domain = CheckpointDomain { chain_id: withdrawal.chain_id, vault: withdrawal.vault };
        verify(&withdrawal, domain, withdrawal.authorization.as_ref().unwrap().signer).unwrap();
        assert_eq!(token_units(withdrawal.amount).unwrap(), U256::from(123_456_789u64));
    }

    #[test]
    fn each_signed_field_and_the_inclusion_proof_reject_tampering() {
        let (withdrawal, _) = vector();
        let domain = CheckpointDomain { chain_id: withdrawal.chain_id, vault: withdrawal.vault };
        let signer = withdrawal.authorization.as_ref().unwrap().signer;
        let mut changes = Vec::new();
        let mut changed = withdrawal.clone();
        changed.account = AccountId([3; 20]);
        changes.push(changed);
        let mut changed = withdrawal.clone();
        changed.amount += Fixed::from_raw(1_000);
        changes.push(changed);
        let mut changed = withdrawal.clone();
        changed.nonce += 1;
        changes.push(changed);
        let mut changed = withdrawal.clone();
        changed.approved_seq = Some(withdrawal.approved_seq.unwrap() + 1);
        changes.push(changed);
        let mut changed = withdrawal.clone();
        changed.authorization.as_mut().unwrap().signer_epoch += 1;
        changes.push(changed);
        let mut changed = withdrawal.clone();
        let auth = changed.authorization.as_mut().unwrap();
        auth.issued_at += 1;
        auth.expires_at += 1;
        changes.push(changed);
        let mut changed = withdrawal.clone();
        changed.authorization.as_mut().unwrap().siblings[0] = B256::ZERO;
        changes.push(changed);
        for changed in changes {
            assert!(verify(&changed, domain, signer).is_err(), "accepted tampered authorization: {changed:?}");
        }
        assert!(verify(&withdrawal, CheckpointDomain { chain_id: domain.chain_id + 1, ..domain }, signer).is_err());
        assert!(verify(&withdrawal, CheckpointDomain { vault: AccountId([4; 20]), ..domain }, signer).is_err());
        assert!(verify(&withdrawal, domain, AccountId([5; 20])).is_err());
    }

    #[test]
    fn non_whole_amounts_and_invalid_lifetimes_cannot_be_signed() {
        for raw in [0, -1_000, 1, 999, 1_001] {
            assert!(token_units(Fixed::from_raw(raw)).is_err(), "accepted amount {raw}");
        }
        let (mut withdrawal, _) = vector();
        withdrawal.authorization.as_mut().unwrap().expires_at += 1;
        assert!(digest(&withdrawal).is_err());
        withdrawal.authorization.as_mut().unwrap().issued_at = u64::MAX;
        assert!(digest(&withdrawal).is_err());
    }
}
