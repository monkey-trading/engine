use super::*;
use engine_wire::FRAME_VERSION;

use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, signing};

const CHALLENGE: [u8; 32] = [0x8f; 32];
const BODY: &[u8] = br#"[{"id":1,"mark":"100000"}]"#;

#[test]
fn a_saved_response_verifies_with_header_names_in_any_case_after_an_informational_one() {
    let headers = format!("HTTP/1.1 100 Continue\r\n\r\n{}", saved_headers("HTTP/1.1 200 OK", 200, BODY));
    let headers = headers.replace("x-engine-", "X-ENGINE-");
    let verified = verify_saved_response(&expected(), "GET", "/v1/markets", &headers, BODY).unwrap();
    assert_eq!(verified, VerifiedResponse { status: 200, time_ms: 1_793_145_600_000, head_seq: 77 });
    // An HTTP/2 status line carries no reason phrase.
    let http2 = saved_headers("HTTP/2 200", 200, BODY);
    assert!(verify_saved_response(&expected(), "GET", "/v1/markets", &http2, BODY).is_ok());
}

#[test]
fn a_saved_response_fails_when_anything_it_was_signed_over_differs() {
    let headers = saved_headers("HTTP/1.1 200 OK", 200, BODY);
    let other_challenge = Expected { challenge: [0x90; 32], ..expected() };
    let other_signer = Expected { signer: AccountId([0x11; 20]), ..expected() };
    let other_chain = Expected { chain_id: 1, ..expected() };
    let unechoed: String =
        headers.lines().filter(|l| !l.starts_with(CHALLENGE_HEADER)).map(|l| format!("{l}\n")).collect();
    for (what, expected, method, path, headers, body, error) in [
        (
            "body",
            expected(),
            "GET",
            "/v1/markets",
            headers.clone(),
            &br#"[{"id":1,"mark":"100001"}]"#[..],
            "not the attested key",
        ),
        (
            "status",
            expected(),
            "GET",
            "/v1/markets",
            headers.replace("200 OK", "404 Not Found"),
            BODY,
            "not the attested key",
        ),
        ("path", expected(), "GET", "/v1/house", headers.clone(), BODY, "not the attested key"),
        ("method", expected(), "POST", "/v1/markets", headers.clone(), BODY, "not the attested key"),
        ("echoed challenge", other_challenge, "GET", "/v1/markets", headers.clone(), BODY, "echoes another challenge"),
        (
            "challenge",
            Expected { challenge: [0x90; 32], ..expected() },
            "GET",
            "/v1/markets",
            unechoed,
            BODY,
            "not the attested key",
        ),
        ("signer", other_signer, "GET", "/v1/markets", headers.clone(), BODY, "not the attested key"),
        ("chain id", other_chain, "GET", "/v1/markets", headers.clone(), BODY, "not the attested key"),
        (
            "time",
            expected(),
            "GET",
            "/v1/markets",
            headers.replace("1793145600000", "1793145600001"),
            BODY,
            "not the attested key",
        ),
    ] {
        let err = verify_saved_response(&expected, method, path, &headers, body).unwrap_err();
        assert!(format!("{err:#}").contains(error), "{what}: {err:#}");
    }
}

#[test]
fn a_response_without_its_signature_headers_or_with_one_twice_is_refused() {
    let headers = saved_headers("HTTP/1.1 200 OK", 200, BODY);
    let without =
        |name: &str| headers.lines().filter(|l| !l.starts_with(name)).map(|l| format!("{l}\n")).collect::<String>();
    for name in [SIGNATURE_HEADER, TIME_HEADER, SEQ_HEADER] {
        let err = verify_saved_response(&expected(), "GET", "/v1/markets", &without(name), BODY).unwrap_err();
        assert!(format!("{err:#}").contains("unsigned"), "{name}: {err:#}");
    }
    let twice = headers.replacen(TIME_HEADER, &format!("{TIME_HEADER}: 1\r\n{TIME_HEADER}"), 1);
    let err = verify_saved_response(&expected(), "GET", "/v1/markets", &twice, BODY).unwrap_err();
    assert!(format!("{err:#}").contains("appears twice"), "{err:#}");
    let err = verify_saved_response(&expected(), "GET", "/v1/markets", "x-engine-seq: 1\n", BODY).unwrap_err();
    assert!(format!("{err:#}").contains("no status line"), "{err:#}");
}

#[test]
fn a_saved_connection_verifies_and_a_removed_or_garbled_frame_fails_at_its_line() {
    let frames = saved_frames(4);
    assert_eq!(verify_saved_frames(&expected(), &frames.join("\n")).unwrap(), 4);

    let mut gap = frames.clone();
    gap.remove(2);
    let err = verify_saved_frames(&expected(), &gap.join("\n")).unwrap_err();
    assert_eq!(format!("{err:#}"), "line 3: frame 3 arrived where frame 2 was due");

    let mut garbled = frames.clone();
    garbled[1] = garbled[1].replace(r#""n":1"#, r#""n":"one""#);
    let err = verify_saved_frames(&expected(), &garbled.join("\n")).unwrap_err();
    assert!(format!("{err:#}").starts_with("line 2: not a signed frame"), "{err:#}");

    let other = Expected { challenge: [0x90; 32], ..expected() };
    let err = verify_saved_frames(&other, &frames.join("\n")).unwrap_err();
    assert_eq!(format!("{err:#}"), "line 1: the signature is not the attested key's over these bytes");

    assert_eq!(format!("{:#}", verify_saved_frames(&expected(), "\n").unwrap_err()), "no frames");
}

#[test]
fn a_challenge_argument_is_64_hex_characters() {
    assert_eq!(parse_challenge_arg(&"8f".repeat(32)), Ok(CHALLENGE));
    assert!(parse_challenge_arg(&format!("0x{}", "8f".repeat(32))).is_err());
}

// ── Helpers ──

/// The dev node's signer and deployment, under `CHALLENGE`.
fn expected() -> Expected {
    let cfg = dev_config();
    Expected {
        signer: AccountId(EnclaveKey::test_key().address()),
        chain_id: cfg.custody.chain_id,
        vault: cfg.custody.vault.parse().unwrap(),
        challenge: CHALLENGE,
    }
}

/// The headers the dev node sends with `body` to `GET /v1/markets`, as `curl -D` saves them.
fn saved_headers(status_line: &str, status: u16, body: &[u8]) -> String {
    let (time_ms, head_seq) = (1_793_145_600_000, 77);
    let response = Response {
        challenge: CHALLENGE,
        time_ms,
        head_seq,
        status,
        method: "GET",
        path_and_query: "/v1/markets",
        body,
    };
    let signature = signing().sign_response(&response);
    format!(
        "{status_line}\r\ncontent-type: application/json\r\n{SIGNATURE_HEADER}: {signature}\r\n{TIME_HEADER}: {time_ms}\r\n\
         {SEQ_HEADER}: {head_seq}\r\n{CHALLENGE_HEADER}: {}\r\n\r\n",
        hex::encode(CHALLENGE)
    )
}

/// The dev node's first `count` frames on a connection opened with `CHALLENGE`, one JSON a line.
fn saved_frames(count: u64) -> Vec<String> {
    (0..count)
        .map(|n| {
            let (time, msg) = (1_793_145_600_000 + n, format!(r#"{{"type":"heartbeat","head":"{n}:0"}}"#));
            let sig = signing().sign_frame(&CHALLENGE, n, time, &msg);
            serde_json::to_string(&SignedFrame { v: FRAME_VERSION, n, time, msg, sig }).unwrap()
        })
        .collect()
}
