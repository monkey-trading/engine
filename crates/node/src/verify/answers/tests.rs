use super::*;
use engine_wire::FRAME_VERSION;

use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, signing};

const CHALLENGE: [u8; 32] = [0x8f; 32];
const ORDER: &[u8] = br#"{"account":"0x0707070707070707070707070707070707070707","nonce":5}"#;
const BODY: &[u8] = br#"{"error":"invalid nonce","expected":7}"#;
const STREAM: &str = "/v1/stream?after=12:0&signed=1&challenge=8f";

#[test]
fn a_saved_response_verifies_with_header_names_in_any_case_after_an_informational_one() {
    let headers = format!("HTTP/1.1 100 Continue\r\n\r\n{}", saved_headers("HTTP/1.1 409 Conflict", 409, BODY));
    let headers = headers.replace("x-engine-", "X-ENGINE-");
    let verified = verify_saved_response(&expected(), &order(ORDER), &headers, BODY).unwrap();
    assert_eq!(verified, VerifiedResponse { status: 409, time_ms: 1_793_145_600_000, head_seq: 77 });
    // An HTTP/2 status line carries no reason phrase.
    let http2 = saved_headers("HTTP/2 409", 409, BODY);
    assert!(verify_saved_response(&expected(), &order(ORDER), &http2, BODY).is_ok());
}

#[test]
fn a_saved_response_fails_when_anything_it_was_signed_over_differs() {
    let headers = saved_headers("HTTP/1.1 409 Conflict", 409, BODY);
    let other_challenge = Expected { challenge: [0x90; 32], ..expected() };
    let unechoed: String =
        headers.lines().filter(|l| !l.starts_with(CHALLENGE_HEADER)).map(|l| format!("{l}\n")).collect();
    let refused = "not the attested key";
    let other_order = br#"{"account":"0x0808080808080808080808080808080808080808","nonce":5}"#;
    let get = SentRequest { method: "GET", ..order(ORDER) };
    let path = SentRequest { path_and_query: "/v1/orders/cancel", ..order(ORDER) };
    for (what, expected, request, headers, body, error) in [
        ("body", expected(), order(ORDER), headers.clone(), &br#"{"error":"x","expected":7}"#[..], refused),
        ("request body", expected(), order(other_order), headers.clone(), BODY, refused),
        ("status", expected(), order(ORDER), headers.replace("409 Conflict", "200 OK"), BODY, refused),
        ("path", expected(), path, headers.clone(), BODY, refused),
        ("method", expected(), get, headers.clone(), BODY, refused),
        ("echoed challenge", other_challenge, order(ORDER), headers.clone(), BODY, "echoes another challenge"),
        ("challenge", Expected { challenge: [0x90; 32], ..expected() }, order(ORDER), unechoed, BODY, refused),
        (
            "signer",
            Expected { signer: AccountId([0x11; 20]), ..expected() },
            order(ORDER),
            headers.clone(),
            BODY,
            refused,
        ),
        ("chain id", Expected { chain_id: 1, ..expected() }, order(ORDER), headers.clone(), BODY, refused),
        ("time", expected(), order(ORDER), headers.replace("1793145600000", "1793145600001"), BODY, refused),
    ] {
        let err = verify_saved_response(&expected, &request, &headers, body).unwrap_err();
        assert!(format!("{err:#}").contains(error), "{what}: {err:#}");
    }
}

#[test]
fn a_response_without_its_signature_headers_or_with_one_twice_is_refused() {
    let headers = saved_headers("HTTP/1.1 409 Conflict", 409, BODY);
    let without =
        |name: &str| headers.lines().filter(|l| !l.starts_with(name)).map(|l| format!("{l}\n")).collect::<String>();
    for name in [SIGNATURE_HEADER, TIME_HEADER, SEQ_HEADER] {
        let err = verify_saved_response(&expected(), &order(ORDER), &without(name), BODY).unwrap_err();
        assert!(format!("{err:#}").contains("unsigned"), "{name}: {err:#}");
    }
    let twice = headers.replacen(TIME_HEADER, &format!("{TIME_HEADER}: 1\r\n{TIME_HEADER}"), 1);
    let err = verify_saved_response(&expected(), &order(ORDER), &twice, BODY).unwrap_err();
    assert!(format!("{err:#}").contains("appears twice"), "{err:#}");
    let err = verify_saved_response(&expected(), &order(ORDER), "x-engine-seq: 1\n", BODY).unwrap_err();
    assert!(format!("{err:#}").contains("no status line"), "{err:#}");
}

#[test]
fn a_saved_connection_verifies_and_a_removed_garbled_or_misattributed_frame_fails_at_its_line() {
    let frames = saved_frames(4);
    assert_eq!(verify_saved_frames(&expected(), STREAM, &frames.join("\n")).unwrap(), 4);

    let mut gap = frames.clone();
    gap.remove(2);
    let err = verify_saved_frames(&expected(), STREAM, &gap.join("\n")).unwrap_err();
    assert_eq!(format!("{err:#}"), "line 3: frame 3 arrived where frame 2 was due");

    let mut garbled = frames.clone();
    garbled[1] = garbled[1].replace(r#""n":1"#, r#""n":"one""#);
    let err = verify_saved_frames(&expected(), STREAM, &garbled.join("\n")).unwrap_err();
    assert!(format!("{err:#}").starts_with("line 2: not a signed frame"), "{err:#}");

    let refused = "line 1: the signature is not the attested key's over these bytes";
    let other = Expected { challenge: [0x90; 32], ..expected() };
    assert_eq!(format!("{:#}", verify_saved_frames(&other, STREAM, &frames.join("\n")).unwrap_err()), refused);
    let rewritten = STREAM.replace("after=12:0", "after=40:0");
    let err = verify_saved_frames(&expected(), &rewritten, &frames.join("\n")).unwrap_err();
    assert_eq!(format!("{err:#}"), refused);

    assert_eq!(format!("{:#}", verify_saved_frames(&expected(), STREAM, "\n").unwrap_err()), "no frames");
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

/// `POST /v1/orders` of `body`.
fn order(body: &[u8]) -> SentRequest<'_> {
    SentRequest { method: "POST", path_and_query: "/v1/orders", body }
}

/// The headers the dev node sends with `body` in answer to `order(ORDER)`, as `curl -D` saves them.
fn saved_headers(status_line: &str, status: u16, body: &[u8]) -> String {
    let (time_ms, head_seq) = (1_793_145_600_000, 77);
    let response = Response {
        challenge: CHALLENGE,
        time_ms,
        head_seq,
        status,
        method: "POST",
        path_and_query: "/v1/orders",
        request_body: ORDER,
        body,
    };
    let signature = signing().sign_response(&response);
    format!(
        "{status_line}\r\ncontent-type: application/json\r\n{SIGNATURE_HEADER}: {signature}\r\n{TIME_HEADER}: {time_ms}\r\n\
         {SEQ_HEADER}: {head_seq}\r\n{CHALLENGE_HEADER}: {}\r\n\r\n",
        hex::encode(CHALLENGE)
    )
}

/// The dev node's first `count` frames on the connection `GET` of `STREAM` opened with `CHALLENGE`,
/// one JSON a line.
fn saved_frames(count: u64) -> Vec<String> {
    let (connection, mut previous) = (Connection::new(CHALLENGE, STREAM), [0; 32]);
    (0..count)
        .map(|n| {
            let (time, msg) = (1_793_145_600_000 + n, format!(r#"{{"type":"heartbeat","head":"{n}:0"}}"#));
            let (digest, sig) = signing().sign_frame(&connection, &previous, n, time, &msg);
            previous = digest;
            serde_json::to_string(&SignedFrame { v: FRAME_VERSION, n, time, msg, sig }).unwrap()
        })
        .collect()
}
