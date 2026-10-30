//! The handover's pieces: what each step binds, the encryption of the keys, the framing, and what
//! each kind of evidence proves. `predecessor`'s tests run the whole handover over loopback.

use super::*;

use engine_core::types::AccountId;
use engine_verify::fixtures::{
    LAUNCH_DIGEST, POLICY, REPORTED_TCB, collateral_of, crl_revoking, forged_report, pem_wrap, test_chain,
};
use hpke::Serializable;

use crate::config::TeeProvider;
use crate::handover::evidence::{Peer, Quoting, verify};
use crate::tee::MockAttestor;
use crate::tee::sealing::Platform;

const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31337, vault: AccountId([0x0a; 20]) };
const CONFIG: [u8; 32] = [0xc1; 32];
const CHALLENGE: [u8; 32] = [0x5c; 32];

#[test]
fn the_keys_open_only_for_the_requested_key_deployment_and_request() {
    let keys = KeySet::generate().unwrap();
    let (secret, public) = Kx::gen_keypair();
    let request = request_report_data(DOMAIN, &CHALLENGE, &public.to_bytes(), &CONFIG);
    let (encapped, ciphertext) = seal_keys(DOMAIN, &public.to_bytes(), &request, &keys).unwrap();
    assert_eq!(open_keys(DOMAIN, &secret, &request, &encapped, &ciphertext).unwrap(), keys);

    let (other_secret, _) = Kx::gen_keypair();
    let other_vault = CheckpointDomain { vault: AccountId([0x0b; 20]), ..DOMAIN };
    let mut other_request = request;
    other_request[0] ^= 1;
    let mut tampered = ciphertext.clone();
    tampered[0] ^= 1;
    for (what, opened) in [
        ("another secret key", open_keys(DOMAIN, &other_secret, &request, &encapped, &ciphertext)),
        ("another deployment", open_keys(other_vault, &secret, &request, &encapped, &ciphertext)),
        ("another request", open_keys(DOMAIN, &secret, &other_request, &encapped, &ciphertext)),
        ("an altered ciphertext", open_keys(DOMAIN, &secret, &request, &encapped, &tampered)),
    ] {
        assert!(opened.is_err(), "the keys opened under {what}");
    }
    assert!(seal_keys(DOMAIN, &[1, 2, 3], &request, &keys).is_err(), "sealed to a malformed key");
}

#[test]
fn each_step_binds_its_deployment_and_what_it_answers() {
    let (key, other_key) = ([1u8; 32], [2u8; 32]);
    let other_chain = CheckpointDomain { chain_id: 1, ..DOMAIN };
    let other_vault = CheckpointDomain { vault: AccountId([0x0b; 20]), ..DOMAIN };
    let request = request_report_data(DOMAIN, &CHALLENGE, &key, &CONFIG);
    assert_eq!(request[32..], [0u8; 32], "the second half of report data is zero");
    for other in [
        request_report_data(DOMAIN, &CHALLENGE, &other_key, &CONFIG),
        request_report_data(DOMAIN, &CHALLENGE, &key, &[0xc2; 32]),
        request_report_data(DOMAIN, &[0x5d; 32], &key, &CONFIG),
        request_report_data(other_chain, &CHALLENGE, &key, &CONFIG),
        request_report_data(other_vault, &CHALLENGE, &key, &CONFIG),
    ] {
        assert_ne!(other, request);
    }

    let keys = keys_report_data(DOMAIN, &request, b"enc", b"cipher");
    assert_eq!(keys[32..], [0u8; 32]);
    assert_ne!(keys, request, "the two steps share a tag");
    for other in [
        keys_report_data(DOMAIN, &request_report_data(DOMAIN, &CHALLENGE, &other_key, &CONFIG), b"enc", b"cipher"),
        keys_report_data(DOMAIN, &request, b"enX", b"cipher"),
        keys_report_data(DOMAIN, &request, b"enc", b"cipheX"),
        keys_report_data(other_vault, &request, b"enc", b"cipher"),
    ] {
        assert_ne!(other, keys);
    }

    let confirm = confirm_digest(DOMAIN, &request);
    assert_ne!(confirm, confirm_digest(other_chain, &request));
    assert_ne!(confirm, confirm_digest(DOMAIN, &request_report_data(DOMAIN, &CHALLENGE, &other_key, &CONFIG)));
}

#[tokio::test]
async fn a_message_over_the_limit_or_cut_short_is_refused() {
    let (mut client, mut server) = tokio::io::duplex(64);
    send(&mut client, &Confirm { signature: "0xab".into() }).await.unwrap();
    assert_eq!(receive::<Confirm>(&mut server).await.unwrap().signature, "0xab");

    let (mut client, mut server) = tokio::io::duplex(64);
    client.write_all(&(MAX_MESSAGE_BYTES + 1).to_be_bytes()).await.unwrap();
    let err = receive::<Confirm>(&mut server).await.unwrap_err();
    assert!(err.to_string().contains("over the limit"), "{err:#}");

    let (mut client, mut server) = tokio::io::duplex(64);
    client.write_all(&10u32.to_be_bytes()).await.unwrap();
    client.write_all(b"{}").await.unwrap();
    drop(client);
    assert!(receive::<Confirm>(&mut server).await.is_err(), "a message cut short parsed");
}

#[tokio::test]
async fn mock_evidence_binds_its_data_and_never_passes_for_a_real_enclave() {
    let platform = Platform { launch_digest: [0x22; 48], policy: 0, tcb: 0 };
    let quoting = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "https://kds.test" };
    let report_data = request_report_data(DOMAIN, &CHALLENGE, &[7u8; 32], &CONFIG);
    let evidence = evidence::produce(&quoting, &MockAttestor, &platform, report_data).await.unwrap();

    let peer = verify(TeeProvider::Mock, &evidence, &report_data, None, None).unwrap();
    assert_eq!(peer, Peer { launch_digest: [0x22; 48], policy: 0, tcb: None });
    assert!(verify(TeeProvider::Mock, &evidence, &[0u8; 64], None, None).is_err(), "it verified other data");
    let err = verify(TeeProvider::ConfigfsTsm, &evidence, &report_data, Some(0), None).unwrap_err();
    assert!(err.to_string().contains("never hands over to a mock one"), "{err:#}");
}

#[test]
fn hardware_evidence_verifies_against_the_vendor_chain_and_binds_its_data() {
    let chip = [0x5a; 64];
    let chain = test_chain(chip, REPORTED_TCB);
    let report_data = request_report_data(DOMAIN, &CHALLENGE, &[7u8; 32], &CONFIG);
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let evidence: Evidence = serde_json::from_value(serde_json::json!({
        "provider": "sev_guest",
        "quote": hex::encode(forged_report(&chain, LAUNCH_DIGEST, report_data, chip)),
        "collateral": String::from_utf8(collateral.to_json()).unwrap(),
    }))
    .unwrap();
    let root = pem_wrap(&collateral.root, "CERTIFICATE");
    let now = Some(1_800_000_000);

    let peer = verify(TeeProvider::ConfigfsTsm, &evidence, &report_data, now, Some(&root)).unwrap();
    assert_eq!(peer, Peer { launch_digest: LAUNCH_DIGEST, policy: POLICY, tcb: Some(REPORTED_TCB) });
    assert!(verify(TeeProvider::ConfigfsTsm, &evidence, &[0u8; 64], now, Some(&root)).is_err(), "other data");
    assert!(verify(TeeProvider::ConfigfsTsm, &evidence, &report_data, now, None).is_err(), "an unpinned root");
    assert!(verify(TeeProvider::ConfigfsTsm, &evidence, &report_data, None, Some(&root)).is_err(), "no time");
    let err = verify(TeeProvider::Mock, &evidence, &report_data, None, None).unwrap_err();
    assert!(err.to_string().contains("mock enclaves only"), "{err:#}");
}
