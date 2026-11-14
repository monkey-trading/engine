//! A member attests a statement only when its replica verified that checkpoint, with that link and
//! root, in that turn assignment, at that turn's boundary, and its quote binds exactly what it was
//! asked.

use super::*;

use engine_core::eip712::B256;

use crate::handover::evidence::verify;
use crate::log_check::Statement;
use crate::tee::MockAttestor;
use crate::test_support::dev_config;

const DIGEST: [u8; 48] = [0x33; 48];
const TURN: u64 = 20_755;

#[tokio::test]
async fn a_member_attests_only_the_checkpoint_its_replica_verified_in_that_turn() {
    let checked = CheckedHeads::default();
    checked.record(Checked { head: head(40), taken: Some((TURN, 1)) });
    let answering = answering(checked);
    let nonce = [0x5c; 32];
    let asked = statement(head(40));
    let Answer::Verified { config_sha256, evidence } = answering.answer(&ask(&nonce, asked)).await else {
        panic!("the member refused a checkpoint its replica verified");
    };
    let config: [u8; 32] = hex::decode(config_sha256).unwrap().try_into().unwrap();
    assert_eq!(config, dev_config().file_sha256);
    let bound = report_data(answering.domain, &nonce, &asked, &config);
    assert_eq!(verify(TeeProvider::Mock, &evidence, &bound, None, None).unwrap().launch_digest, DIGEST);
    let other = report_data(answering.domain, &nonce, &statement(head(41)), &config);
    assert!(verify(TeeProvider::Mock, &evidence, &other, None, None).is_err(), "the quote binds another statement");

    let refused = [
        ("another root", statement(VerifiedHead { root: B256::repeat_byte(9), ..head(40) }), "another log"),
        ("another link", statement(VerifiedHead { link: B256::repeat_byte(9), ..head(40) }), "another log"),
        ("a checkpoint not yet verified", statement(head(50)), "not yet seq 50"),
        ("a checkpoint the replica does not hold", statement(head(30)), "retains no checkpoint at seq 30"),
        ("another turn", Statement { turn: TURN + 1, boundary: boundary(TURN + 1), ..asked }, "not turn"),
        ("another position", Statement { position: 0, ..asked }, "not turn"),
        ("another boundary", Statement { boundary: boundary(TURN) + 1, ..asked }, "boundary"),
    ];
    for (what, statement, says) in refused {
        match answering.answer(&ask(&nonce, statement)).await {
            Answer::Refused { reason } => assert!(reason.contains(says), "{what}: {reason}"),
            Answer::Verified { .. } => panic!("{what} was attested"),
        }
    }
    let malformed = Ask { nonce: "00".into(), statement: asked };
    assert!(matches!(answering.answer(&malformed).await, Answer::Refused { .. }), "a short challenge was attested");
}

#[test]
fn the_checked_heads_keep_the_latest_and_a_rebuild_replaces_what_it_replays_again() {
    let checked = CheckedHeads::default();
    for seq in [10, 20, 30] {
        checked.record(Checked { head: head(seq), taken: None });
    }
    // A rebuilt replica replays from the start: a checkpoint at or before the last drops the later ones.
    checked.record(Checked { head: head(20), taken: Some((TURN, 0)) });
    assert_eq!(checked.at(30), None);
    assert_eq!(checked.at(20).and_then(|checked| checked.taken), Some((TURN, 0)));
    assert_eq!(checked.latest(), Some(20));
    for seq in 21..(21 + RETAINED as u64) {
        checked.record(Checked { head: head(seq), taken: None });
    }
    assert_eq!((checked.at(10), checked.at(20)), (None, None), "more than the retained checkpoints are kept");
    assert!(checked.at(21).is_some());
}

#[tokio::test]
async fn a_member_answers_over_its_endpoint_while_it_runs_and_frees_it_once_dropped() {
    let checked = CheckedHeads::default();
    checked.record(Checked { head: head(40), taken: Some((TURN, 1)) });
    let answering = answering(checked);
    let listen = answering.listen;
    let nonce = [0x7a; 32];
    let asking = async {
        let mut stream = loop {
            match TcpStream::connect(listen).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        };
        send(&mut stream, &ask(&nonce, statement(head(40)))).await.unwrap();
        receive::<Answer>(&mut stream).await.unwrap()
    };
    let answer = tokio::select! {
        answer = asking => answer,
        never = answering.run() => match never {},
    };
    assert!(matches!(answer, Answer::Verified { .. }), "{answer:?}");
    // Dropped with the member's wait, the endpoint is free for the handover listener.
    TcpListener::bind(listen).await.expect("the endpoint is still bound");
}

// ── Helpers ──

/// A member's answers from `checked`, quoting under the mock TEE as `DIGEST`, on a free endpoint.
fn answering(checked: CheckedHeads) -> Answering {
    let mut cfg = dev_config();
    let listen = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    cfg.tee.sealing = Some(toml::from_str(&format!("dir = '/nonexistent'\n[handover]\nlisten = '{listen}'")).unwrap());
    let platform = Platform { launch_digest: DIGEST, policy: 0, tcb: 0 };
    Answering::new(&cfg, Box::new(MockAttestor), platform, checked).unwrap().expect("[tee.sealing.handover] is set")
}

/// The statement of `head` at `TURN`'s boundary, position 1.
fn statement(head: VerifiedHead) -> Statement {
    Statement { signer_epoch: 1, boundary: boundary(TURN), turn: TURN, position: 1, set: 1, head }
}

/// The question of `statement` under `nonce`.
fn ask(nonce: &[u8; 32], statement: Statement) -> Ask {
    Ask { nonce: hex::encode(nonce), statement }
}

/// A checkpoint at `seq`.
fn head(seq: u64) -> VerifiedHead {
    VerifiedHead { seq, link: B256::repeat_byte(seq as u8), root: B256::repeat_byte(!(seq as u8)) }
}

/// When `turn` starts.
fn boundary(turn: u64) -> engine_core::types::Millis {
    engine_core::turns::boundary(turn).unwrap()
}
