use super::*;
use k256::ecdsa::SigningKey;

const DEPLOYMENT: Deployment = Deployment { chain_id: 31337, vault: [0x5f; 20] };
const CHALLENGE: [u8; 32] = [0xc4; 32];

#[test]
fn a_response_verifies_only_under_every_field_it_was_signed_over() {
    let key = key(0x43);
    let base = response(br#"[{"id":1}]"#);
    let signature = sign(&key, response_digest(&DEPLOYMENT, &base));
    assert_eq!(verify_response(&signer(&key), &DEPLOYMENT, &base, &signature), Ok(()));

    let other_chain = Deployment { chain_id: 1, ..DEPLOYMENT };
    let other_vault = Deployment { vault: [0x60; 20], ..DEPLOYMENT };
    let changed: [(&str, Deployment, Response<'_>); 10] = [
        ("body", DEPLOYMENT, Response { body: br#"[{"id":2}]"#, ..base }),
        ("status", DEPLOYMENT, Response { status: 500, ..base }),
        ("path", DEPLOYMENT, Response { path_and_query: "/v1/house", ..base }),
        ("method", DEPLOYMENT, Response { method: "POST", ..base }),
        // The zero byte keeps the method and path from trading characters.
        ("method and path boundary", DEPLOYMENT, Response { method: "GET/v1", path_and_query: "/markets", ..base }),
        ("challenge", DEPLOYMENT, Response { challenge: [0xc5; 32], ..base }),
        ("time", DEPLOYMENT, Response { time_ms: base.time_ms + 1, ..base }),
        ("head seq", DEPLOYMENT, Response { head_seq: base.head_seq + 1, ..base }),
        ("chain id", other_chain, base),
        ("vault", other_vault, base),
    ];
    for (what, deployment, response) in changed {
        assert_eq!(
            verify_response(&signer(&key), &deployment, &response, &signature),
            Err(Error::WrongSigner),
            "a changed {what} still verified"
        );
    }
}

#[test]
fn a_response_signed_by_another_key_fails() {
    let response = response(b"[]");
    let signature = sign(&key(0x44), response_digest(&DEPLOYMENT, &response));
    assert_eq!(verify_response(&signer(&key(0x43)), &DEPLOYMENT, &response, &signature), Err(Error::WrongSigner));
}

#[test]
fn a_malformed_signature_is_refused() {
    let response = response(b"[]");
    let good = sign(&key(0x43), response_digest(&DEPLOYMENT, &response));
    let with_v = |v: &str| format!("{}{v}", &good[..good.len() - 2]);
    for bad in [
        good.trim_start_matches("0x").to_owned(),
        good[..good.len() - 2].to_owned(),
        format!("{good}00"),
        good.replacen('a', "g", 1).replacen('b', "g", 1),
        with_v("1d"),
        with_v("00"),
        format!("0x{}1b", "00".repeat(64)),
    ] {
        assert_eq!(
            verify_response(&signer(&key(0x43)), &DEPLOYMENT, &response, &bad),
            Err(Error::MalformedSignature),
            "{bad}"
        );
    }
}

#[test]
fn a_challenge_is_exactly_64_hex_characters() {
    assert_eq!(parse_challenge(&"c4".repeat(32)), Some(CHALLENGE));
    assert_eq!(parse_challenge(&"C4".repeat(32)), Some(CHALLENGE));
    assert_eq!(parse_challenge(&"0".repeat(64)), Some([0; 32]));
    for bad in [
        "c4".repeat(31) + "c",
        "c4".repeat(32) + "c",
        format!("0x{}", "c4".repeat(31)),
        format!("+f{}", "c4".repeat(31)),
        "g4".repeat(32),
        String::new(),
    ] {
        assert_eq!(parse_challenge(&bad), None, "{bad}");
    }
}

#[test]
fn frames_verify_from_zero_in_order() {
    let key = key(0x43);
    let mut verifier = FrameVerifier::new(signer(&key), DEPLOYMENT, CHALLENGE);
    for frame in frames(&key, DEPLOYMENT, CHALLENGE, 3) {
        assert_eq!(verifier.verify(&frame), Ok(()));
    }
}

#[test]
fn a_dropped_repeated_or_reordered_frame_fails_where_it_breaks_the_sequence() {
    let key = key(0x43);
    let sent = frames(&key, DEPLOYMENT, CHALLENGE, 3);
    for (what, order, fails_at, error) in [
        ("dropped", vec![0, 2], 1, Error::OutOfSequence { expected: 1, got: 2 }),
        ("repeated", vec![0, 1, 1], 2, Error::OutOfSequence { expected: 2, got: 1 }),
        ("reordered", vec![1, 0], 0, Error::OutOfSequence { expected: 0, got: 1 }),
    ] {
        let mut verifier = FrameVerifier::new(signer(&key), DEPLOYMENT, CHALLENGE);
        for (at, index) in order.into_iter().enumerate() {
            let result = verifier.verify(&sent[index]);
            if at < fails_at {
                assert_eq!(result, Ok(()), "{what}: frame {at}");
            } else {
                assert_eq!(result, Err(error), "{what}: frame {at}");
                break;
            }
        }
    }
}

#[test]
fn a_frame_from_another_connection_deployment_or_key_fails() {
    let key = key(0x43);
    for (what, frame) in [
        ("challenge", frames(&key, DEPLOYMENT, [0xc5; 32], 1).remove(0)),
        ("chain id", frames(&key, Deployment { chain_id: 1, ..DEPLOYMENT }, CHALLENGE, 1).remove(0)),
        ("vault", frames(&key, Deployment { vault: [0x60; 20], ..DEPLOYMENT }, CHALLENGE, 1).remove(0)),
        ("key", frames(&self::key(0x44), DEPLOYMENT, CHALLENGE, 1).remove(0)),
    ] {
        let mut verifier = FrameVerifier::new(signer(&key), DEPLOYMENT, CHALLENGE);
        assert_eq!(verifier.verify(&frame), Err(Error::WrongSigner), "a frame for another {what} verified");
    }
}

#[test]
fn a_frame_with_an_altered_message_time_or_version_fails() {
    let key = key(0x43);
    let frame = frames(&key, DEPLOYMENT, CHALLENGE, 1).remove(0);
    for (what, altered, error) in [
        ("message", SignedFrame { msg: frame.msg.replace("0:0", "0:1"), ..frame.clone() }, Error::WrongSigner),
        ("time", SignedFrame { time: frame.time + 1, ..frame.clone() }, Error::WrongSigner),
        ("version", SignedFrame { v: 2, ..frame.clone() }, Error::UnsupportedVersion(2)),
    ] {
        let mut verifier = FrameVerifier::new(signer(&key), DEPLOYMENT, CHALLENGE);
        assert_eq!(verifier.verify(&altered), Err(error), "an altered {what}");
    }
}

#[test]
fn once_a_frame_fails_no_later_frame_verifies() {
    let key = key(0x43);
    let sent = frames(&key, DEPLOYMENT, CHALLENGE, 2);
    let mut verifier = FrameVerifier::new(signer(&key), DEPLOYMENT, CHALLENGE);
    let forged = SignedFrame { msg: "{}".into(), ..sent[0].clone() };
    assert_eq!(verifier.verify(&forged), Err(Error::WrongSigner));
    // The genuine frame 0, and then frame 1, would otherwise verify.
    assert_eq!(verifier.verify(&sent[0]), Err(Error::ConnectionFailed));
    assert_eq!(verifier.verify(&sent[1]), Err(Error::ConnectionFailed));
}

#[test]
fn the_two_tags_are_distinct_and_neither_prefixes_the_other() {
    assert!(!RESPONSE_TAG.starts_with(FRAME_TAG) && !FRAME_TAG.starts_with(RESPONSE_TAG));
}

// ── Helpers ──

fn key(byte: u8) -> SigningKey {
    SigningKey::from_slice(&[byte; 32]).unwrap()
}

fn signer(key: &SigningKey) -> [u8; 20] {
    address(key.verifying_key())
}

/// `key`'s recoverable signature over `digest`, as the node writes it.
fn sign(key: &SigningKey, digest: [u8; 32]) -> String {
    let (signature, recovery_id) = key.sign_prehash_recoverable(&digest);
    format!("0x{}{:02x}", hex::encode(signature.to_bytes()), 27 + recovery_id.to_byte())
}

fn response(body: &[u8]) -> Response<'_> {
    Response {
        challenge: CHALLENGE,
        time_ms: 1_792_540_800_000,
        head_seq: 42,
        status: 200,
        method: "GET",
        path_and_query: "/v1/markets",
        body,
    }
}

/// The first `count` frames of a connection `key` signs for `deployment` and `challenge`.
fn frames(key: &SigningKey, deployment: Deployment, challenge: [u8; 32], count: u64) -> Vec<SignedFrame> {
    (0..count)
        .map(|n| {
            let (time, msg) = (1_792_540_800_000 + n, format!(r#"{{"offset":"{n}:0","type":"command"}}"#));
            let sig = sign(key, frame_digest(&deployment, &challenge, n, time, &msg));
            SignedFrame { v: FRAME_VERSION, n, time, msg, sig }
        })
        .collect()
}
