use super::*;

fn key(byte: u8) -> SigningKey {
    SigningKey::from_slice(&[byte; 32]).unwrap()
}

#[test]
fn a_signature_recovers_to_its_signer_only_for_the_hash_it_signed() {
    let (key, hash) = (key(7), B256::repeat_byte(1));
    let signature = sign(&key, &hash);
    assert_eq!(recover(&hash, &signature), Some(address(key.verifying_key())));
    assert_ne!(recover(&B256::repeat_byte(2), &signature), Some(address(key.verifying_key())));
}

#[test]
fn a_malformed_signature_recovers_nobody() {
    let hash = B256::repeat_byte(1);
    let mut signature = sign(&key(7), &hash);
    signature[64] = 1;
    assert_eq!(recover(&hash, &signature), None);
    assert_eq!(recover(&hash, &Signature::ZERO), None);
}

#[test]
fn the_domain_binds_the_chain_and_vault() {
    let vault = AccountId([9; 20]);
    let base = domain(1, vault).separator();
    assert_ne!(base, domain(2, vault).separator());
    assert_ne!(base, domain(1, AccountId([8; 20])).separator());
}
