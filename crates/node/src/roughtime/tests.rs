//! The client over replies recorded from the public servers and replies a local server signs.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;

use super::*;
use crate::roughtime::testing::{ReplyFields, TestServer};

/// Replies recorded from the public servers in `config/dev.toml`, with the nonce each answers and
/// the fields a separate parser read from it.
const RECORDED: &str = include_str!("fixtures/replies.json");
const NONCE: [u8; 32] = [7; 32];
const MIDPOINT_S: u64 = 1_792_502_815;

#[derive(Deserialize)]
struct Recorded {
    replies: Vec<RecordedReply>,
}

#[derive(Deserialize)]
struct RecordedReply {
    server: String,
    public_key: String,
    version: u32,
    midpoint_s: u64,
    radius_s: u32,
    nonce: String,
    reply: String,
}

#[test]
fn recorded_replies_from_the_public_servers_verify_under_both_versions() {
    let recorded = recorded();
    for r in &recorded {
        let key = key(&r.public_key);
        let request = Request::new(&key, hex::decode(&r.nonce).unwrap().try_into().unwrap());
        let reading =
            request.verify(&hex::decode(&r.reply).unwrap(), &key).unwrap_or_else(|e| panic!("{}: {e:#}", r.server));
        assert_eq!(
            reading,
            Reading { midpoint_s: r.midpoint_s, radius_s: r.radius_s, version: r.version },
            "{}",
            r.server
        );
    }
    let versions: Vec<u32> = recorded.iter().map(|r| r.version).collect();
    assert!(versions.contains(&VERSION_1) && versions.contains(&VERSION_DRAFT), "{versions:x?}");
}

#[test]
fn a_recorded_reply_is_refused_for_another_nonce_or_another_servers_key() {
    let recorded = recorded();
    let (r, other) = (&recorded[0], &recorded[1]);
    let (key, reply) = (key(&r.public_key), hex::decode(&r.reply).unwrap());
    let err = Request::new(&key, [0; 32]).verify(&reply, &key).unwrap_err();
    assert!(format!("{err:#}").contains("another request"), "{err:#}");

    let nonce = hex::decode(&r.nonce).unwrap().try_into().unwrap();
    let wrong_key = self::key(&other.public_key);
    let err = Request::new(&wrong_key, nonce).verify(&reply, &wrong_key).unwrap_err();
    assert!(format!("{err:#}").contains("delegation not signed"), "{err:#}");
}

#[test]
fn a_request_is_a_1024_byte_message_offering_both_versions_to_one_key() {
    let server = TestServer::new(1, VERSION_1);
    let request = Request::new(&server.public_key(), NONCE);
    let message = Message::parse(unframe(request.packet()).unwrap()).unwrap();
    assert_eq!(unframe(request.packet()).unwrap().len(), 1024);
    assert_eq!(message.get(Tag::VER).unwrap(), [1, 0, 0, 0, 0x0c, 0, 0, 0x80]);
    assert_eq!(message.get_array(Tag::SRV).unwrap(), hash(&[&[0xff], server.public_key().as_bytes()]));
    assert_eq!(message.get(Tag::NONC).unwrap(), NONCE);
    assert_eq!(message.get_u32(Tag::TYPE).unwrap(), 0);
    assert!(message.get(Tag::ZZZZ).unwrap().iter().all(|b| *b == 0));
}

#[test]
fn a_signed_reply_verifies_in_either_version() {
    for version in [VERSION_1, VERSION_DRAFT] {
        let server = TestServer::new(1, version);
        let request = Request::new(&server.public_key(), NONCE);
        let reading = request.verify(&server.reply(request.packet(), MIDPOINT_S, 3), &server.public_key()).unwrap();
        assert_eq!(reading, Reading { midpoint_s: MIDPOINT_S, radius_s: 3, version });
    }
}

#[test]
fn a_reply_signed_under_the_other_versions_context_strings_is_refused() {
    for (version, signed_as) in [(VERSION_1, VERSION_DRAFT), (VERSION_DRAFT, VERSION_1)] {
        let err = tampered(version, |f| f.signed_as = signed_as).unwrap_err();
        assert!(format!("{err:#}").contains("not signed"), "{err:#}");
    }
}

#[test]
fn a_reply_altered_after_signing_is_refused() {
    let server = TestServer::new(1, VERSION_1);
    let request = Request::new(&server.public_key(), NONCE);
    let mut reply = server.reply(request.packet(), MIDPOINT_S, 3);
    let midpoint = MIDPOINT_S.to_le_bytes();
    let at = reply.windows(8).position(|w| w == midpoint).unwrap();
    reply[at] ^= 1;
    let err = request.verify(&reply, &server.public_key()).unwrap_err();
    assert!(format!("{err:#}").contains("response not signed by the delegated key"), "{err:#}");
}

#[test]
fn a_midpoint_outside_the_delegation_window_is_refused() {
    let err = tampered(VERSION_1, |f| f.mint = MIDPOINT_S + 1).unwrap_err();
    assert!(format!("{err:#}").contains("outside the delegation"), "{err:#}");
    let err = tampered(VERSION_1, |f| f.maxt = MIDPOINT_S - 1).unwrap_err();
    assert!(format!("{err:#}").contains("outside the delegation"), "{err:#}");
    assert!(tampered(VERSION_1, |f| (f.mint, f.maxt) = (MIDPOINT_S, MIDPOINT_S)).is_ok());
}

#[test]
fn a_zero_radius_a_request_type_or_an_unoffered_version_is_refused() {
    let err = tampered(VERSION_1, |f| f.radius_s = 0).unwrap_err();
    assert!(format!("{err:#}").contains("zero radius"), "{err:#}");
    let err = tampered(VERSION_1, |f| f.message_type = 0).unwrap_err();
    assert!(format!("{err:#}").contains("not a response"), "{err:#}");
    let err = tampered(VERSION_1, |f| f.version = 0x8000_000b).unwrap_err();
    assert!(format!("{err:#}").contains("not offered"), "{err:#}");
}

#[test]
fn a_merkle_path_proves_the_request_on_either_side() {
    let sibling = [9; 32];
    let leaf = |f: &ReplyFields| f.root;
    let left =
        tampered(VERSION_1, |f| (f.root, f.path, f.index) = (hash(&[&[1], &leaf(f), &sibling]), sibling.to_vec(), 0));
    assert!(left.is_ok(), "{left:?}");
    let right =
        tampered(VERSION_1, |f| (f.root, f.path, f.index) = (hash(&[&[1], &sibling, &leaf(f)]), sibling.to_vec(), 1));
    assert!(right.is_ok(), "{right:?}");
    let swapped =
        tampered(VERSION_1, |f| (f.root, f.path, f.index) = (hash(&[&[1], &sibling, &leaf(f)]), sibling.to_vec(), 0));
    assert!(format!("{:#}", swapped.unwrap_err()).contains("not in the signed Merkle tree"));
}

#[test]
fn a_merkle_path_that_is_malformed_or_shallower_than_its_index_is_refused() {
    let err = tampered(VERSION_1, |f| f.index = 1).unwrap_err();
    assert!(format!("{err:#}").contains("deeper than its path"), "{err:#}");
    let err = tampered(VERSION_1, |f| f.path = vec![0; 36]).unwrap_err();
    assert!(format!("{err:#}").contains("bad Merkle path length"), "{err:#}");
    let err = tampered(VERSION_1, |f| f.path = vec![0; 33 * 32]).unwrap_err();
    assert!(format!("{err:#}").contains("bad Merkle path length"), "{err:#}");
}

#[test]
fn a_32_level_merkle_path_with_the_top_index_bit_set_verifies() {
    let request = [3u8; 16];
    let path: Vec<u8> = (0..32u8).flat_map(|i| [i; 32]).collect();
    let root =
        path.as_chunks::<32>().0.iter().fold(hash(&[&[0], &request]), |node, sibling| hash(&[&[1], sibling, &node]));
    verify_inclusion(&request, &path, u32::MAX, root).unwrap();
}

#[test]
fn messages_round_trip_and_ignore_unknown_tags() {
    let grease = Tag::new(*b"GRSE");
    let bytes = encode(vec![(Tag::NONC, &NONCE[..]), (grease, &[1, 2, 3, 4][..]), (Tag::TYPE, &[1, 0, 0, 0][..])]);
    let message = Message::parse(&bytes).unwrap();
    assert_eq!(message.get(Tag::NONC).unwrap(), NONCE);
    assert_eq!(message.get_u32(Tag::TYPE).unwrap(), 1);
    assert!(message.get(Tag::PATH).is_err());
    assert_eq!(unframe(&frame(&bytes)).unwrap(), bytes);
}

#[test]
fn malformed_messages_and_packets_are_refused() {
    let good = encode(vec![(Tag::NONC, &NONCE[..]), (Tag::TYPE, &[1, 0, 0, 0][..])]);
    let cases: [(&str, Vec<u8>); 7] = [
        ("truncated", vec![2, 0]),
        ("no pairs", vec![0, 0, 0, 0]),
        ("header too long", vec![9, 0, 0, 0, 0, 0, 0, 0]),
        ("unaligned offset", with_word(&good, 4, 3)),
        ("offset past the end", with_word(&good, 4, 40)),
        ("tags out of order", with_word(&good, 8, u32::from_le_bytes(*b"ZZZZ"))),
        ("a tag twice", with_word(&good, 12, u32::from_le_bytes(*b"NONC"))),
    ];
    for (case, bytes) in cases {
        assert!(Message::parse(&bytes).is_err(), "{case}");
    }
    let mut packet = frame(&good);
    assert!(unframe(&packet[..10]).is_err());
    packet.push(0);
    assert!(format!("{:#}", unframe(&packet).unwrap_err()).contains("length field"));
    packet[0] = b'X';
    assert!(format!("{:#}", unframe(&packet).unwrap_err()).contains("not a Roughtime packet"));
}

// ── Helpers ──

/// The recorded replies.
fn recorded() -> Vec<RecordedReply> {
    serde_json::from_str::<Recorded>(RECORDED).unwrap().replies
}

/// A base64 Ed25519 public key.
fn key(base64: &str) -> VerifyingKey {
    VerifyingKey::from_bytes(&STANDARD.decode(base64).unwrap().try_into().unwrap()).unwrap()
}

/// Verifies a reply to a fresh request after `tamper` changes its fields.
fn tampered(version: u32, tamper: impl FnOnce(&mut ReplyFields)) -> anyhow::Result<Reading> {
    let server = TestServer::new(1, version);
    let request = Request::new(&server.public_key(), NONCE);
    let mut fields = server.fields(request.packet(), MIDPOINT_S, 3);
    tamper(&mut fields);
    request.verify(&server.sign(&fields), &server.public_key())
}

/// `bytes` with the little-endian word at `at` replaced by `word`.
fn with_word(bytes: &[u8], at: usize, word: u32) -> Vec<u8> {
    let mut out = bytes.to_vec();
    out[at..at + 4].copy_from_slice(&word.to_le_bytes());
    out
}
