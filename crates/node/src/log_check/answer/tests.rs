//! A member attests a statement only when the question is signed by the checkpoint's signer and its
//! replica verified that checkpoint, with that link and root, in that turn assignment, at that turn's
//! boundary, and its quote binds exactly what it was asked; an idle connection holds up no other.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

use engine_core::eip712::B256;

use crate::handover::evidence::verify;
use crate::log_check::Statement;
use crate::tee::{EnclaveKey, MockAttestor};
use crate::test_support::dev_config;

const DIGEST: [u8; 48] = [0x33; 48];
pub(crate) const TURN: u64 = 20_755;

#[tokio::test]
async fn a_member_attests_only_the_checkpoint_its_replica_verified_in_that_turn() {
    let checked = CheckedHeads::default();
    checked.record(Checked { head: head(40), taken: Some((TURN, 1)), signer: operator() });
    let answering = answering(checked);
    let nonce = [0x5c; 32];
    let asked = statement(head(40));
    let Answer::Verified { config_sha256, evidence } = answering.answer(&ask(&nonce, asked)).await else {
        panic!("the member refused a checkpoint its replica verified");
    };
    let config: [u8; 32] = hex::decode(config_sha256).unwrap().try_into().unwrap();
    assert_eq!(config, dev_config().file_sha256);
    let bound = report_data(answering.domain, &nonce, &asked, &config);
    assert_eq!(verify(TeeProvider::Mock, &[], &evidence, &bound, None, None).unwrap().launch_digest, DIGEST);
    let other = report_data(answering.domain, &nonce, &statement(head(41)), &config);
    assert!(
        verify(TeeProvider::Mock, &[], &evidence, &other, None, None).is_err(),
        "the quote binds another statement"
    );

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
    let malformed = Ask { nonce: "00".into(), ..ask(&nonce, asked) };
    assert!(matches!(answering.answer(&malformed).await, Answer::Refused { .. }), "a short challenge was attested");

    // Only the enclave that signed the checkpoint makes the member quote.
    let other = EnclaveKey::generate();
    let unsigned = [
        ("no signature", Ask { signature: "0x00".into(), ..ask(&nonce, asked) }, "65 bytes"),
        ("another key", Ask { signature: signed_by(&other, &nonce, &asked), ..ask(&nonce, asked) }, "not signed by"),
        (
            "another question",
            Ask { signature: ask(&[0x11; 32], asked).signature, ..ask(&nonce, asked) },
            "not signed by",
        ),
    ];
    for (what, question, says) in unsigned {
        match answering.answer(&question).await {
            Answer::Refused { reason } => assert!(reason.contains(says), "{what}: {reason}"),
            Answer::Verified { .. } => panic!("{what} was attested"),
        }
    }
}

#[test]
fn the_checked_heads_keep_the_latest_and_a_rebuild_replaces_what_it_replays_again() {
    let checked = CheckedHeads::default();
    for seq in [10, 20, 30] {
        checked.record(Checked { head: head(seq), taken: None, signer: operator() });
    }
    // A rebuilt replica replays from the start: a checkpoint at or before the last drops the later ones.
    checked.record(Checked { head: head(20), taken: Some((TURN, 0)), signer: operator() });
    assert_eq!(checked.at(30), None);
    assert_eq!(checked.at(20).and_then(|checked| checked.taken), Some((TURN, 0)));
    assert_eq!(checked.latest(), Some(20));
    for seq in 21..(21 + RETAINED as u64) {
        checked.record(Checked { head: head(seq), taken: None, signer: operator() });
    }
    assert_eq!((checked.at(10), checked.at(20)), (None, None), "more than the retained checkpoints are kept");
    assert!(checked.at(21).is_some());
}

#[tokio::test]
async fn a_member_answers_over_its_endpoint_while_it_runs_past_an_idle_connection_and_frees_it_once_dropped() {
    let checked = CheckedHeads::default();
    checked.record(Checked { head: head(40), taken: Some((TURN, 1)), signer: operator() });
    let answering = answering(checked);
    let listen = answering.listen;
    let nonce = [0x7a; 32];
    let asking = async {
        // A connection that sends nothing holds up no other, and is closed once its deadline passes.
        let mut idle = connect(listen).await;
        let mut stream = connect(listen).await;
        send(&mut stream, &ask(&nonce, statement(head(40)))).await.unwrap();
        let answer = receive::<Answer>(&mut stream).await.unwrap();
        let closed = timeout(QUESTION_TIMEOUT * 4, receive::<Answer>(&mut idle)).await;
        assert!(closed.is_ok_and(|read| read.is_err()), "the idle connection stayed open past its deadline");
        answer
    };
    let answer = tokio::select! {
        answer = asking => answer,
        never = answering.run() => match never {},
    };
    assert!(matches!(answer, Answer::Verified { .. }), "{answer:?}");
    // Dropped with the member's wait, the endpoint is free for the handover listener.
    TcpListener::bind(listen).await.expect("the endpoint is still bound");
}

#[tokio::test]
async fn one_address_holds_at_most_two_connections_so_it_cannot_fill_the_endpoint() {
    let answering = answering(CheckedHeads::default());
    let listen = answering.listen;
    let probing = async {
        let (mut first, _second) = (connect(listen).await, connect(listen).await);
        let _third = connect(listen).await;
        // The third takes the place of the address's oldest silent one at once, not at its question's
        // deadline.
        let read = timeout(QUESTION_TIMEOUT / 2, receive::<Answer>(&mut first)).await;
        assert!(read.is_ok_and(|read| read.is_err()), "the address's oldest silent connection kept its place");
    };
    tokio::select! {
        () = probing => {}
        never = answering.run() => match never {},
    }
}

/// Counts the quotes it produces.
struct Counting(Arc<AtomicUsize>);

impl Attestor for Counting {
    fn attest(&self, report_data: [u8; 64]) -> anyhow::Result<(String, Vec<u8>)> {
        self.0.fetch_add(1, Ordering::SeqCst);
        MockAttestor.attest(report_data)
    }
}

/// A question replayed, however many times and however fast, is quoted once: the replays get the
/// evidence already produced, and the challenge answers no other question.
#[tokio::test]
async fn a_replayed_question_is_answered_with_the_evidence_already_produced() {
    let checked = CheckedHeads::default();
    checked.record(Checked { head: head(40), taken: Some((TURN, 1)), signer: operator() });
    let quotes = Arc::new(AtomicUsize::new(0));
    let answering = answering_with(checked, Box::new(Counting(quotes.clone())));
    let (nonce, question) = ([0x21; 32], statement(head(40)));
    let questions: Vec<Ask> = (0..8).map(|_| ask(&nonce, question)).collect();
    let replays = futures_util::future::join_all(questions.iter().map(|question| answering.answer(question))).await;
    assert!(replays.iter().all(|answer| matches!(answer, Answer::Verified { .. })), "{replays:?}");
    assert_eq!(quotes.load(Ordering::SeqCst), 1, "a replay made the member quote again");
    // The same challenge under a statement the signer also signed is not the question it answered.
    let checked = CheckedHeads::default();
    checked.record(Checked { head: head(40), taken: Some((TURN, 1)), signer: operator() });
    checked.record(Checked { head: head(41), taken: Some((TURN, 2)), signer: operator() });
    let answering = answering_with(checked, Box::new(Counting(quotes.clone())));
    assert!(matches!(answering.answer(&ask(&nonce, question)).await, Answer::Verified { .. }));
    let other = Statement { head: head(41), position: 2, ..question };
    assert!(
        matches!(answering.answer(&ask(&nonce, other)).await, Answer::Refused { reason } if reason.contains("another question"))
    );
}

/// However many distinct questions the signer signs, a member quotes at most `QUOTES_PER_MINUTE` times.
#[tokio::test]
async fn a_member_quotes_at_most_a_set_number_of_times_a_minute() {
    let checked = CheckedHeads::default();
    checked.record(Checked { head: head(40), taken: Some((TURN, 1)), signer: operator() });
    let quotes = Arc::new(AtomicUsize::new(0));
    let answering = answering_with(checked, Box::new(Counting(quotes.clone())));
    let mut refused = 0;
    for n in 0..=QUOTES_PER_MINUTE as u8 {
        if matches!(answering.answer(&ask(&[n; 32], statement(head(40)))).await, Answer::Refused { .. }) {
            refused += 1;
        }
    }
    assert_eq!((quotes.load(Ordering::SeqCst), refused), (QUOTES_PER_MINUTE, 1));
}

/// A table full of connections that have not asked makes room for the operator's: the oldest silent
/// one is dropped.
#[tokio::test]
async fn a_full_table_drops_the_oldest_silent_connection_for_the_operators_question() {
    let checked = CheckedHeads::default();
    checked.record(Checked { head: head(40), taken: Some((TURN, 1)), signer: operator() });
    let mut answering = answering(checked);
    // Every connection here shares one address, so let it hold the whole table.
    (answering.connections, answering.per_peer) = (2, 3);
    let listen = answering.listen;
    let asking = async {
        let (mut first, _second) = (connect(listen).await, connect(listen).await);
        let mut stream = connect(listen).await;
        send(&mut stream, &ask(&[0x31; 32], statement(head(40)))).await.unwrap();
        let answer = receive::<Answer>(&mut stream).await.unwrap();
        let dropped = timeout(QUESTION_TIMEOUT / 2, receive::<Answer>(&mut first)).await;
        assert!(dropped.is_ok_and(|read| read.is_err()), "the oldest silent connection was kept");
        answer
    };
    let answer = tokio::select! {
        answer = asking => answer,
        never = answering.run() => match never {},
    };
    assert!(matches!(answer, Answer::Verified { .. }), "{answer:?}");
}

// ── Helpers ──

/// A connection to `listen`, once the member's endpoint accepts them.
async fn connect(listen: SocketAddr) -> TcpStream {
    loop {
        match TcpStream::connect(listen).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

/// A member's answers from `checked`, quoting under the mock TEE as `DIGEST`, on a free endpoint.
pub(crate) fn answering(checked: CheckedHeads) -> Answering {
    answering_with(checked, Box::new(MockAttestor))
}

/// `answering`, quoting with `attestor`.
fn answering_with(checked: CheckedHeads, attestor: Box<dyn Attestor>) -> Answering {
    let mut cfg = dev_config();
    let listen = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    cfg.tee.sealing = Some(toml::from_str(&format!("dir = '/nonexistent'\n[handover]\nlisten = '{listen}'")).unwrap());
    let platform = Platform { launch_digest: DIGEST, policy: 0, tcb: 0 };
    Answering::new(&cfg, attestor, platform, checked).unwrap().expect("[tee.sealing.handover] is set")
}

/// The statement of `head` at `TURN`'s boundary, position 1.
pub(crate) fn statement(head: VerifiedHead) -> Statement {
    Statement { signer_epoch: 1, boundary: boundary(TURN), turn: TURN, position: 1, set: 1, head }
}

/// The operator's question of `statement` under `nonce`, signed by the operator enclave's key.
pub(crate) fn ask(nonce: &[u8; 32], statement: Statement) -> Ask {
    Ask { nonce: hex::encode(nonce), statement, signature: signed_by(&EnclaveKey::test_key(), nonce, &statement) }
}

/// `key`'s signature over the question of `statement` under `nonce`.
fn signed_by(key: &EnclaveKey, nonce: &[u8; 32], statement: &Statement) -> String {
    key.sign_digest(ask_digest(domain(), nonce, statement).0)
}

/// The deployment `dev_config` names, which the member answers in.
fn domain() -> CheckpointDomain {
    let cfg = dev_config();
    CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() }
}

/// The operator enclave whose key signed the checkpoints, and signs the questions.
pub(crate) fn operator() -> AccountId {
    AccountId(EnclaveKey::test_key().address())
}

/// A checkpoint at `seq`.
pub(crate) fn head(seq: u64) -> VerifiedHead {
    VerifiedHead { seq, link: B256::repeat_byte(seq as u8), root: B256::repeat_byte(!(seq as u8)) }
}

/// When `turn` starts.
fn boundary(turn: u64) -> engine_core::types::Millis {
    engine_core::turns::boundary(turn).unwrap()
}
