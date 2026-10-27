use engine_core::access::{EligibilityRequest, Screening};
use engine_core::eip712;
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use serde_json::Value;

#[test]
fn independent_wallet_typed_data_matches_both_eligibility_classes_and_source_evidence() {
    let vectors: Value = serde_json::from_str(include_str!("../fixtures/access-eip712.json")).unwrap();
    let account: AccountId = serde_json::from_value(vectors["account"].clone()).unwrap();
    let key = SigningKey::from_slice(
        &hex::decode(vectors["private_key"].as_str().unwrap().trim_start_matches("0x")).unwrap(),
    )
    .unwrap();
    let domain = eip712::domain(
        vectors["chain_id"].as_u64().unwrap(),
        serde_json::from_value(vectors["vault"].clone()).unwrap(),
    );
    assert_eq!(vectors["declarations"].as_array().unwrap().len(), 2);
    for vector in vectors["declarations"].as_array().unwrap() {
        let request: EligibilityRequest = serde_json::from_value(vector["body"].clone()).unwrap();
        assert_eq!(vector["typed_data"]["message"]["statement"], request.class.statement());
        let hash = eip712::eligibility_hash(
            &domain,
            request.account,
            request.class,
            request.auth.nonce,
            request.auth.expires_at,
        );
        assert_eq!(eip712::recover(&hash, &request.auth.signature), Some(account));
        assert_eq!(
            eip712::sign(&key, &hash),
            request.auth.signature,
            "cast wallet and static Rust EIP712 structs agree"
        );
    }
    for vector in vectors["screenings"].as_array().unwrap() {
        let screening: Screening = serde_json::from_value(vector["body"].clone()).unwrap();
        let hash = eip712::screening_hash(&domain, &screening);
        assert_eq!(eip712::recover(&hash, &screening.signature), Some(account));
        assert_eq!(eip712::sign(&key, &hash), screening.signature);
    }
}
