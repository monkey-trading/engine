//! The EIP-712 vectors clients sign against. `fixtures/eip712.json` holds, for each request type,
//! the API body, the typed data a wallet signs with `eth_signTypedData_v4`, and the signature of
//! the Anvil test key over that typed data, made by Foundry's `cast wallet sign --data`. Signing
//! is deterministic (RFC 6979), so the engine reproducing a signature from the body shows that the
//! typed data and the engine's structs gave the same digest when it was signed. The test also
//! hashes the published typed data as a wallet does, and checks its message against the body, so a
//! change to either side fails here.

use alloy_dyn_abi::TypedData;
use alloy_primitives::hex;
use engine_core::command::{AdminCommand, CancelRequest, OrderKind, OrderRequest, WithdrawalRequest};
use engine_core::eip712::{
    self, Signature, admin_hash, cancel_hash, order_hash, session_key_registration_hash, session_key_revocation_hash,
    withdrawal_hash,
};
use engine_core::types::{AccountId, Millis};
use k256::ecdsa::SigningKey;
use serde::Deserialize;
use serde_json::{Value, json};

const VECTORS: &str = include_str!("fixtures/eip712.json");

#[derive(Deserialize)]
struct Vectors {
    private_key: String,
    account: AccountId,
    chain_id: u64,
    vault: AccountId,
    requests: Vec<Vector>,
}

/// A session-key registration or revocation body, as the API takes it.
#[derive(Deserialize)]
struct SessionKeyBody {
    account: AccountId,
    session_key: AccountId,
    #[serde(default)]
    expires_at: Millis,
}

#[derive(Deserialize)]
struct Vector {
    kind: String,
    nonce: u64,
    expires_at: Millis,
    body: Value,
    typed_data: Value,
    signature: Signature,
}

#[test]
fn the_engine_reproduces_each_vector_signature_from_its_request_body() {
    let vectors: Vectors = serde_json::from_str(VECTORS).unwrap();
    let key = SigningKey::from_slice(&hex::decode(&vectors.private_key).unwrap()).unwrap();
    assert_eq!(eip712::address(key.verifying_key()), vectors.account);
    let domain = eip712::domain(vectors.chain_id, vectors.vault);
    for vector in &vectors.requests {
        let (nonce, expires_at) = (vector.nonce, vector.expires_at);
        // The digest the engine signs from the body, and the wallet message the body stands for.
        let (hash, message) = match vector.kind.as_str() {
            "order" => {
                let req: OrderRequest = serde_json::from_value(vector.body.clone()).unwrap();
                let (kind, price, max_slippage_bps) = match req.kind {
                    OrderKind::Market { quoted, max_slippage_bps } => ("market", quoted, max_slippage_bps),
                    OrderKind::Limit { price } => ("limit", price, None),
                    OrderKind::Stop { price } => ("stop", price, None),
                };
                let message = json!({
                    "account": req.account.to_string(), "nonce": nonce, "expiresAt": expires_at,
                    "clientId": req.client_id, "market": req.market, "side": req.side,
                    "qty": req.qty.raw().to_string(), "kind": kind, "price": price.raw().to_string(),
                    "hasMaxSlippage": max_slippage_bps.is_some(), "maxSlippageBps": max_slippage_bps.unwrap_or(0),
                    "reduceOnly": req.reduce_only,
                });
                (order_hash(&domain, &req, nonce, expires_at), message)
            }
            "cancel" => {
                let body: CancelRequest = serde_json::from_value(vector.body.clone()).unwrap();
                let message = json!({
                    "account": body.account.to_string(), "nonce": nonce, "expiresAt": expires_at,
                    "orderId": body.order_id,
                });
                (cancel_hash(&domain, body.account, body.order_id, nonce, expires_at), message)
            }
            "withdrawal" => {
                let body: WithdrawalRequest = serde_json::from_value(vector.body.clone()).unwrap();
                let message = json!({
                    "account": body.account.to_string(), "nonce": nonce, "expiresAt": expires_at,
                    "amount": body.amount.raw().to_string(),
                });
                (withdrawal_hash(&domain, body.account, body.amount, nonce, expires_at), message)
            }
            "register_session_key" => {
                let body: SessionKeyBody = serde_json::from_value(vector.body.clone()).unwrap();
                let message = json!({
                    "account": body.account.to_string(), "nonce": nonce, "expiresAt": expires_at,
                    "sessionKey": body.session_key.to_string(), "sessionExpiresAt": body.expires_at,
                });
                (
                    session_key_registration_hash(
                        &domain,
                        body.account,
                        body.session_key,
                        body.expires_at,
                        nonce,
                        expires_at,
                    ),
                    message,
                )
            }
            "revoke_session_key" => {
                let body: SessionKeyBody = serde_json::from_value(vector.body.clone()).unwrap();
                let message = json!({
                    "account": body.account.to_string(), "nonce": nonce, "expiresAt": expires_at,
                    "sessionKey": body.session_key.to_string(),
                });
                (session_key_revocation_hash(&domain, body.account, body.session_key, nonce, expires_at), message)
            }
            other => panic!("unknown request kind {other}"),
        };
        assert_eq!(eip712::sign(&key, &hash), vector.signature, "{} with nonce {}", vector.kind, vector.nonce);
        assert_eq!(eip712::recover(&hash, &vector.signature), Some(vectors.account));
        // The typed data names this deployment and says exactly what the body asks for, so a client
        // copying it signs the request the engine runs.
        let typed = &vector.typed_data;
        // Hashed as a wallet hashes it: the published types, primary type, and domain give the
        // engine's digest.
        let wallet: TypedData = serde_json::from_value(typed.clone()).unwrap();
        assert_eq!(wallet.eip712_signing_hash().unwrap(), hash, "{} with nonce {}", vector.kind, vector.nonce);
        assert_eq!(typed["domain"]["chainId"], vectors.chain_id);
        assert_eq!(typed["domain"]["verifyingContract"], vectors.vault.to_string());
        assert_eq!(typed["message"], message, "{} with nonce {}", vector.kind, vector.nonce);
    }
}

#[test]
fn an_admin_command_signs_as_its_json_and_hashes_as_a_wallet_hashes_it() {
    let vectors: Vectors = serde_json::from_str(VECTORS).unwrap();
    let command = AdminCommand::SetReduceOnly { market: None, reduce_only: true };
    // The JSON the operator signs and a wallet shows. Changing how a command encodes changes what
    // every logged signature covers, so a replay would no longer verify it.
    let json = r#"{"action":"set_reduce_only","market":null,"reduce_only":true}"#;
    assert_eq!(serde_json::to_string(&command).unwrap(), json);
    let typed = json!({
        "types": {
            "EIP712Domain": [
                { "name": "name", "type": "string" },
                { "name": "version", "type": "string" },
                { "name": "chainId", "type": "uint256" },
                { "name": "verifyingContract", "type": "address" },
            ],
            "AdminRequest": [
                { "name": "nonce", "type": "uint64" },
                { "name": "expiresAt", "type": "int64" },
                { "name": "command", "type": "string" },
            ],
        },
        "primaryType": "AdminRequest",
        "domain": {
            "name": eip712::DOMAIN_NAME,
            "version": eip712::DOMAIN_VERSION,
            "chainId": vectors.chain_id,
            "verifyingContract": vectors.vault.to_string(),
        },
        "message": { "nonce": 3, "expiresAt": 1_792_540_800_000i64, "command": json },
    });
    let wallet: TypedData = serde_json::from_value(typed).unwrap();
    let domain = eip712::domain(vectors.chain_id, vectors.vault);
    assert_eq!(wallet.eip712_signing_hash().unwrap(), admin_hash(&domain, &command, 3, 1_792_540_800_000));
    assert_ne!(
        admin_hash(&domain, &command, 4, 1_792_540_800_000),
        admin_hash(&domain, &command, 3, 1_792_540_800_000)
    );
}

// Foundry cast 1.8.3 `wallet sign --no-hash`, using the public test key 0x0707…07.
// The final three vectors retain the pre-upgrade RFC6979 signatures for digests >= the order.
const PREHASH_VECTORS: &[(&str, &str)] = &[
    (
        "0x0000000000000000000000000000000000000000000000000000000000000000",
        "0x71d86ada313901044c512fea29792a3cab47f7fa53515fa80eb0a9771ce28d5425e51a5cb3435b1a95ce37097493899675bf3cbdc9fbcbf6a9d6d6cbbc1e91ed1c",
    ),
    (
        "0x0000000000000000000000000000000000000000000000000000000000000001",
        "0xac2daa261a9f5b23b9ef1d69c1153b6bcab4384296b62c814da81303d4709ed30b25f1b78511048730ab6909a069049712571e5e40298794d37a98adb5dcb2e21b",
    ),
    (
        "0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140",
        "0xc2dbc6b049d980fd004d471bdd64ae49c01d83994f6be15ad1ee4d8d237273db0ebbede04a7a50996f458b4d163b32665f481812da12d5735441bd83098831d01c",
    ),
    (
        "0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141",
        "0xd52b301383894180ff33b1480fe37252042bc74b9e9c362a27d4430b712678481ea6b5aeeac661e7250f98542931ea6fe6535cddc849912ef207858ae77c0d2f1c",
    ),
    (
        "0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364142",
        "0xe250d8a5141a173cdfef762086cdba4156ddab4397ee619b224adc86156c2511477b1adb6204fa7151b862f20808db70e0dac2beabfc6ae9bdd3d78a76af58261c",
    ),
    (
        "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "0x5070d4335545b513bd3d3292eb562341e5b1ac4eaf738438642e7fa11f3ae4c5123dbcb1f2bb9288852297c8a948088bdc61bfe0a1d9b601a5710ca927072b571b",
    ),
];

#[test]
fn prehash_boundary_signatures_remain_compatible_and_are_not_hashed_again() {
    use engine_core::eip712::B256;
    use k256::ecdsa::Signature as EcdsaSignature;
    use k256::ecdsa::signature::hazmat::PrehashVerifier;
    use sha3::{Digest, Keccak256};

    let key = SigningKey::from_slice(&[7; 32]).unwrap();
    let expected: AccountId = "0x4a62316623ad457f02cdc5d997ded67a383ec569".parse().unwrap();
    assert_eq!(eip712::address(key.verifying_key()), expected);
    for (index, &(digest, legacy)) in PREHASH_VECTORS.iter().enumerate() {
        let hash: B256 = digest.parse().unwrap();
        let legacy: Signature = legacy.parse().unwrap();
        assert_eq!(eip712::recover(&hash, &legacy), Some(expected), "historical vector {index}");
        let signed = eip712::sign(&key, &hash);
        let ecdsa = EcdsaSignature::from_slice(&signed[..64]).unwrap();
        key.verifying_key().verify_prehash(hash.as_slice(), &ecdsa).unwrap();
        assert!(key.verifying_key().verify_prehash(&Keccak256::digest(hash), &ecdsa).is_err());
        assert_eq!(eip712::recover(&hash, &signed), Some(expected));
        if index < 3 {
            assert_eq!(signed, legacy, "RFC6979 below the curve order");
        }
    }
    // RFC6979 reduces the prehash modulo the curve order before deriving its nonce.
    let order: B256 = PREHASH_VECTORS[3].0.parse().unwrap();
    assert_eq!(eip712::sign(&key, &order), eip712::sign(&key, &B256::ZERO));
}

#[test]
fn recovery_rejects_high_s_malformed_scalars_and_non_ethereum_recovery_ids() {
    use engine_core::eip712::B256;
    use k256::ecdsa::{RecoveryId, Signature as EcdsaSignature, VerifyingKey};
    use k256::elliptic_curve::PrimeField;

    let key = SigningKey::from_slice(&[7; 32]).unwrap();
    for &(digest, _) in PREHASH_VECTORS {
        let hash: B256 = digest.parse().unwrap();
        let signed = eip712::sign(&key, &hash);
        let low = EcdsaSignature::from_slice(&signed[..64]).unwrap();
        let high = EcdsaSignature::from_scalars(low.r().to_repr(), (-low.s()).to_repr()).unwrap();
        let mut malleated = signed;
        malleated[..64].copy_from_slice(&high.to_bytes());
        malleated[64] = if signed[64] == 27 { 28 } else { 27 };
        // Recovery alone accepts this equivalent signature; verification must refuse it.
        let recovered = VerifyingKey::recover_from_prehash(
            hash.as_slice(),
            &high,
            RecoveryId::from_byte(malleated[64] - 27).unwrap(),
        )
        .unwrap();
        assert_eq!(eip712::address(&recovered), eip712::address(key.verifying_key()));
        assert_eq!(eip712::recover(&hash, &malleated), None, "high-S {digest}");
        for v in 0..=u8::MAX {
            if v == 27 || v == 28 {
                continue;
            }
            let mut bad = signed;
            bad[64] = v;
            assert_eq!(eip712::recover(&hash, &bad), None, "recovery id {v}");
        }
        for offset in [0, 32] {
            for invalid in
                [[0; 32], hex!("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141"), [255; 32]]
            {
                let mut bad = signed;
                bad[offset..offset + 32].copy_from_slice(&invalid);
                assert_eq!(eip712::recover(&hash, &bad), None, "scalar at {offset}");
            }
        }
    }
}
