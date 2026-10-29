use engine_verify::fixtures::*;
use serde::Deserialize;

use super::*;

const VECTORS: &str = include_str!("../../wire/tests/vectors.json");

#[test]
fn an_attestation_verifies_against_a_manifest_list_and_names_the_release_and_window() {
    let (chain, enclave, attestation) = fixtures();
    let previous = release_toml()
        .replace("test-release-v1", "test-release-v0")
        .replace(&hex::encode([0x1d; 48]), &hex::encode([0x0d; 48]));
    let verified = verify_attestation(
        &serde_json::to_string(&attestation).unwrap(),
        vec![previous, release_toml()],
        &collateral(&chain),
        NOW_MS,
        Some(pem(&chain.ark)),
    )
    .unwrap();

    assert!(!verified.mock);
    assert_eq!(verified.release_name.as_deref(), Some("test-release-v1"));
    assert_eq!(verified.config_name.as_deref(), Some("node.toml"));
    assert_eq!(verified.enclave_address, format!("0x{}", "5e".repeat(20)));
    assert_eq!(verified.tls_public_key_sha256, hex0x(&enclave.bundle.tls_public_key_sha256));
    // The fixture list runs 2024-01-01 to 2049-01-01, inside the certificates' window.
    assert_eq!(verified.valid_from_ms, Some(1_704_067_200_000.0));
    assert_eq!(verified.valid_until_ms, Some(2_493_072_000_999.0));
}

#[test]
fn the_returned_window_is_exactly_the_milliseconds_that_verify() {
    let (chain, _enclave, attestation) = fixtures();
    // A list current for one day, 2027-06-01 to 2027-06-02, inside the certificates' window.
    let crl = crl_signed(&chain, &[], (2027, 6, 1), (2027, 6, 2), None);
    let collateral = String::from_utf8(collateral_of(&chain, &crl).to_json()).unwrap();
    let attestation = serde_json::to_string(&attestation).unwrap();
    let verify = |now_ms: f64| {
        verify_attestation(&attestation, vec![release_toml()], &collateral, now_ms, Some(pem(&chain.ark)))
    };
    let verified = verify(1_811_808_000_000.0 + 3_600_000.0).unwrap();
    let (from, until) = (verified.valid_from_ms.unwrap(), verified.valid_until_ms.unwrap());
    assert_eq!((from, until), (1_811_808_000_000.0, 1_811_894_400_999.0));
    for (now_ms, verifies) in [(from - 1.0, false), (from, true), (until, true), (until + 1.0, false)] {
        assert_eq!(verify(now_ms).is_ok(), verifies, "at {now_ms}");
    }
}

#[test]
fn an_attestation_fails_with_the_verifiers_message_and_without_the_extra_root() {
    let (chain, enclave, attestation) = fixtures();
    let mut mock_quote = engine_verify::MOCK_QUOTE_MARKER.to_vec();
    mock_quote.extend_from_slice(&enclave.bundle.report_data());
    let mock = serde_json::to_string(&engine_verify::fixtures::attestation(&enclave.bundle, &mock_quote)).unwrap();
    let real = serde_json::to_string(&attestation).unwrap();
    let verify = |attestation: &str, now_ms: f64, root: Option<String>| {
        verify_attestation(attestation, vec![release_toml()], &collateral(&chain), now_ms, root).err()
    };

    assert_eq!(
        verify(&mock, NOW_MS, Some(pem(&chain.ark))).as_deref(),
        Some(
            "the quote is not an SEV-SNP attestation report; a mock quote never verifies: \
             quote is 91 bytes, not the 1184 of an SEV-SNP attestation report"
        )
    );
    assert_eq!(verify(&real, NOW_MS, None).as_deref(), Some("chain root is not a pinned vendor root"));
    assert_eq!(verify(&real, f64::NAN, Some(pem(&chain.ark))).as_deref(), Some("nowMs is not a finite number"));
    // After the list's next update, 2049-01-01.
    assert_eq!(
        verify(&real, 2_493_072_001_000.0, Some(pem(&chain.ark))).as_deref(),
        Some("revocation list is expired")
    );
    let err = verify_attestation(&real, vec!["name = 1".into()], &collateral(&chain), NOW_MS, None).err().unwrap();
    assert!(err.starts_with("parsing manifest 0: "), "{err}");
}

#[test]
fn a_mock_attestation_reads_only_through_the_mock_entry_point_and_says_so() {
    let (_chain, enclave, attestation) = fixtures();
    let mut mock_quote = engine_verify::MOCK_QUOTE_MARKER.to_vec();
    mock_quote.extend_from_slice(&enclave.bundle.report_data());
    let mock = serde_json::to_string(&engine_verify::fixtures::attestation(&enclave.bundle, &mock_quote)).unwrap();

    let read = verify_mock_attestation(&mock).unwrap();
    assert!(read.mock);
    assert_eq!(
        (read.release_name, read.config_name, read.valid_from_ms, read.valid_until_ms),
        (None, None, None, None)
    );
    assert_eq!(read.enclave_address, hex0x(&enclave.bundle.enclave_address));
    let real = serde_json::to_string(&attestation).unwrap();
    assert_eq!(
        verify_mock_attestation(&real).err().as_deref(),
        Some("the quote is not a mock quote for the report data")
    );
}

#[test]
fn every_response_vector_ends_as_recorded() {
    let vectors: Vectors = serde_json::from_str(VECTORS).unwrap();
    for v in &vectors.responses {
        let expected = ExpectedSigner::new(&vectors.signer, v.chain_id, &v.vault, &v.challenge).unwrap();
        let result = verify_response(
            &expected,
            &v.method,
            &v.path_and_query,
            v.request_body.as_bytes(),
            v.status,
            Some(v.signature.clone()),
            Some(v.time_ms.to_string()),
            Some(v.head_seq.to_string()),
            Some(v.challenge.clone()),
            v.body.as_bytes(),
        );
        match (&v.error, result) {
            (None, Ok(verified)) => {
                assert_eq!(
                    (verified.status, verified.time_ms, verified.head_seq),
                    (v.status, v.time_ms as f64, v.head_seq)
                )
            }
            (Some(kind), Err(message)) => assert_eq!(message, message_of(kind, None), "{}", v.name),
            (expected, result) => panic!("{}: expected {expected:?}, got {:?}", v.name, result.err()),
        }
    }
}

#[test]
fn a_response_without_a_signature_header_or_with_another_challenge_echoed_is_refused() {
    let vectors: Vectors = serde_json::from_str(VECTORS).unwrap();
    let v = vectors.responses.iter().find(|v| v.error.is_none()).unwrap();
    let expected = ExpectedSigner::new(&vectors.signer, v.chain_id, &v.vault, &v.challenge).unwrap();
    let verify = |signature: Option<&str>, time: Option<&str>, seq: Option<&str>, challenge: Option<&str>| {
        verify_response(
            &expected,
            &v.method,
            &v.path_and_query,
            v.request_body.as_bytes(),
            v.status,
            signature.map(str::to_owned),
            time.map(str::to_owned),
            seq.map(str::to_owned),
            challenge.map(str::to_owned),
            v.body.as_bytes(),
        )
        .err()
    };
    let (time, seq) = (v.time_ms.to_string(), v.head_seq.to_string());
    let (sig, time, seq) = (Some(v.signature.as_str()), Some(time.as_str()), Some(seq.as_str()));
    assert_eq!(verify(sig, time, seq, None), None, "the echo is optional");
    assert_eq!(verify(None, time, seq, None).as_deref(), Some("no x-engine-signature: the response is unsigned"));
    assert_eq!(verify(sig, None, seq, None).as_deref(), Some("no x-engine-time: the response is unsigned"));
    assert_eq!(verify(sig, time, None, None).as_deref(), Some("no x-engine-seq: the response is unsigned"));
    assert_eq!(verify(sig, Some("soon"), seq, None).as_deref(), Some("x-engine-time is not a decimal number"));
    assert_eq!(verify(sig, time, Some("-1"), None).as_deref(), Some("x-engine-seq is not a decimal number"));
    assert_eq!(
        verify(sig, time, seq, Some(&"00".repeat(32))).as_deref(),
        Some("the response echoes another challenge than the one sent")
    );
}

#[test]
fn every_connection_vector_verifies_to_its_recorded_frame_and_a_garbled_frame_ends_it() {
    let vectors: Vectors = serde_json::from_str(VECTORS).unwrap();
    for v in &vectors.connections {
        let expected = ExpectedSigner::new(&vectors.signer, v.chain_id, &v.vault, &v.challenge).unwrap();
        let mut verifier = FrameVerifier::new(&expected, &v.path_and_query);
        let results: Vec<Result<String, String>> =
            v.frames.iter().map(|frame| verifier.verify_frame(&frame.to_string())).collect();
        assert_eq!(results.iter().take_while(|r| r.is_ok()).count(), v.verified, "{}", v.name);
        let failed = v.frames.get(v.verified).map(|frame| (v.verified, frame["n"].as_u64().unwrap()));
        let expected_error = v.error.as_deref().map(|kind| message_of(kind, failed));
        assert_eq!(results.iter().find_map(|r| r.clone().err()), expected_error, "{}", v.name);
        for (frame, result) in v.frames.iter().zip(&results).take(v.verified) {
            assert_eq!(result.as_deref().ok(), frame["msg"].as_str(), "{}: the message is returned", v.name);
        }
    }

    let v = vectors.connections.iter().find(|v| v.error.is_none()).unwrap();
    let expected = ExpectedSigner::new(&vectors.signer, v.chain_id, &v.vault, &v.challenge).unwrap();
    let mut verifier = FrameVerifier::new(&expected, &v.path_and_query);
    let err = verifier.verify_frame("not json").unwrap_err();
    assert!(err.starts_with("not a signed frame: "), "{err}");
    let err = verifier.verify_frame(&v.frames[0].to_string()).unwrap_err();
    assert_eq!(err, message_of("connection_failed", None));
}

#[test]
fn malformed_signer_inputs_are_refused() {
    let challenge = "8f".repeat(32);
    let vault = format!("0x{}", "5f".repeat(20));
    assert_eq!(
        ExpectedSigner::new("5e5e", 1, &vault, &challenge).err().as_deref(),
        Some("the signer is not 0x and 40 hex digits")
    );
    assert_eq!(
        ExpectedSigner::new(&vault, 1, "0x5f", &challenge).err().as_deref(),
        Some("the vault is not 0x and 40 hex digits")
    );
    assert!(ExpectedSigner::new(&vault, 1, &vault, &format!("0x{challenge}")).is_err());
    assert!(ExpectedSigner::new(&vault.to_uppercase().replacen("0X", "0x", 1), 1, &vault, &challenge).is_ok());
}

// ── Helpers ──

/// 2027-01-01, inside every fixture's window.
const NOW_MS: f64 = 1_798_761_600_000.0;

#[derive(Deserialize)]
struct Vectors {
    signer: String,
    responses: Vec<ResponseVector>,
    connections: Vec<ConnectionVector>,
}

#[derive(Deserialize)]
struct ResponseVector {
    name: String,
    chain_id: u64,
    vault: String,
    challenge: String,
    time_ms: u64,
    head_seq: u64,
    status: u16,
    method: String,
    path_and_query: String,
    request_body: String,
    body: String,
    signature: String,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ConnectionVector {
    name: String,
    chain_id: u64,
    vault: String,
    challenge: String,
    path_and_query: String,
    frames: Vec<serde_json::Value>,
    verified: usize,
    error: Option<String>,
}

/// The collateral document a node would serve for `chain`.
fn collateral(chain: &TestChain) -> String {
    String::from_utf8(collateral_of(chain, &crl_revoking(chain, &[])).to_json()).unwrap()
}

/// The message `engine_wire` gives for a vector's error kind; an out-of-sequence frame names the
/// place that was due and the `n` that came.
fn message_of(kind: &str, failed: Option<(usize, u64)>) -> String {
    match (kind, failed) {
        ("malformed_signature", _) => "the signature is not 0x and 65 bytes of hex ending in v 27 or 28".into(),
        ("wrong_signer", _) => "the signature is not the attested key's over these bytes".into(),
        ("out_of_sequence", Some((due, got))) => format!("frame {got} arrived where frame {due} was due"),
        ("connection_failed", _) => "an earlier frame on this connection failed".into(),
        (other, _) => panic!("no message for {other}"),
    }
}
