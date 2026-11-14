//! The operator's node admits a member's verification only when its quote binds the statement it
//! asked under its challenge, from the release the member registered, under a policy no weaker than
//! its own, and from a member and release that sequenced none of the range; it asks the qualifying
//! members of the turn's order first to last, signing each question, gives the sequencer the first it
//! admits, and pages `log_check_pending` once while a check stays due.

use super::*;

use std::collections::BTreeSet;
use std::sync::Arc;

use engine_core::Fixed;
use engine_core::types::AccountId;

use crate::checkpoints::{LifecycleEvidence, Observation, VaultState};
use crate::log::Checkpoint;
use crate::log_check::answer::{Answering, Checked, CheckedHeads};
use crate::log_check::{Sequencers, VerifiedHead};
use crate::tee::MockAttestor;
use crate::test_support::{Alert, capture_alerts_and_resolutions, dev_config};

const TURN: u64 = 20_755;
const CONFIG: [u8; 32] = [0xc1; 32];

#[tokio::test]
async fn a_verification_counts_only_from_the_registered_release_outside_the_range_binding_what_was_asked() {
    let own = Platform { launch_digest: [0; 48], policy: 0, tcb: 0 };
    let checks = Checks { domain: domain(), provider: TeeProvider::Mock, own: &own, now_seconds: None };
    let (c, b) = (member(0xc, ""), member(0xb, ""));
    // B's member on another release, as after an upgrade.
    let b_upgraded = HandoverSource { release: release_of(0xd).0, ..member(0xb, "") };
    let nonce = [0x11; 32];
    let asked = statement();
    let excluded = sequencers(&[0xb]);

    let verified = answer(0xc, CONFIG, &nonce, &asked).await;
    assert_eq!(admit(&checks, (&nonce, &asked), &c, &excluded, verified).unwrap(), release_of(0xc));

    let refusals = [
        ("a refusal", Answer::Refused { reason: "not yet".into() }, &c, "refused"),
        ("another challenge", answer(0xc, CONFIG, &[0x22; 32], &asked).await, &c, "binds other data"),
        (
            "another statement",
            answer(0xc, CONFIG, &nonce, &Statement { turn: TURN + 1, ..asked }).await,
            &c,
            "binds other data",
        ),
        ("another member's release", answer(0xc, CONFIG, &nonce, &asked).await, &b, "not 0x"),
        ("another config", answer(0xc, [0xc2; 32], &nonce, &asked).await, &c, "not 0x"),
        // A verification from B's own release, which sequenced the range, is refused, and so is one
        // from B's member on another release.
        ("a release in the range", answer(0xb, CONFIG, &nonce, &asked).await, &b, "sequenced part of the range"),
        (
            "a member in the range",
            answer(0xd, CONFIG, &nonce, &asked).await,
            &b_upgraded,
            "sequenced part of the range",
        ),
    ];
    for (what, answer, from, says) in refusals {
        let err = admit(&checks, (&nonce, &asked), from, &excluded, answer).expect_err(what);
        assert!(format!("{err:#}").contains(says), "{what}: {err:#}");
    }
    let stricter = Platform { policy: 0x30000, ..own };
    let checks = Checks { own: &stricter, ..checks };
    let weaker = admit(&checks, (&nonce, &asked), &c, &excluded, answer(0xc, CONFIG, &nonce, &asked).await);
    assert!(format!("{:#}", weaker.unwrap_err()).contains("guest policy"), "a weaker policy was admitted");
}

#[test]
fn the_node_asks_neither_itself_nor_a_member_or_release_in_the_range_nor_a_member_with_no_endpoint() {
    // F sequenced part of the range on a release it has since left.
    let f = HandoverSource { release: release_of(0x9).0, ..member(0xf, "f:1") };
    let order = [member(0xa, "a:1"), member(0xb, "b:1"), member(0xd, ""), f, member(0xc, "c:1"), member(0xe, "e:1")];
    let excluded =
        Sequencers { candidates: BTreeSet::from([AccountId([0xf; 20])]), releases: BTreeSet::from([release_of(0xe)]) };
    let due = due(AccountId([0xa; 20]), excluded);
    let asked: Vec<_> = qualifying(&order, &due).map(|member| member.candidate).collect();
    assert_eq!(asked, [AccountId([0xb; 20]), AccountId([0xc; 20])]);
}

#[tokio::test]
async fn the_first_qualifying_member_is_asked_first_then_the_next_and_the_sequencer_takes_the_admitted_one() {
    // A asks: with both replicas past the checkpoint, B, first in the order after A, verifies.
    assert_eq!(first_admitted(Some(head())).await, release_of(0xb));
    // B's replica has not verified the checkpoint yet; C, the next qualifying member, has.
    assert_eq!(first_admitted(None).await, release_of(0xc));
}

#[tokio::test(start_paused = true)]
async fn a_check_no_member_verifies_pages_once_and_the_episode_ends_once_it_is_not_due() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    // B, the only other member, registered no handover endpoint: no round admits anything.
    let order = vec![member(0xa, "127.0.0.1:1"), member(0xb, "")];
    let (sequencer, _requests) = mpsc::channel(4);
    let (due_tx, due_rx) = watch::channel(Some(due(AccountId([0xa; 20]), sequencers(&[0xa]))));
    let asker = asker(order, &sequencer, due_rx);
    let drive = async {
        tokio::time::sleep(PENDING_ALERT_AFTER / 2).await;
        assert!(named(&mut alerts, "log_check_pending").is_empty(), "a check due for a moment paged");
        tokio::time::sleep(PENDING_ALERT_AFTER * 4).await;
        let paged = named(&mut alerts, "log_check_pending");
        assert_eq!(paged.len(), 1, "one episode, one page: {paged:?}");
        assert_eq!(paged[0]["turn"], TURN.to_string());
        due_tx.send_replace(None);
        tokio::time::sleep(RETRY).await;
        let ended = named(&mut resolved, "log_check_pending");
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert_eq!(ended[0]["turn"], TURN.to_string());
        assert!(named(&mut alerts, "log_check_pending").is_empty(), "the alert paged again");
    };
    tokio::select! {
        () = drive => {}
        ran = asker.run() => panic!("the asker stopped: {ran:?}"),
    }
}

// ── Helpers ──

/// The schedule's order of one turn, read at an observation whose signer epoch is 1.
struct Order {
    order: Vec<HandoverSource>,
}

impl CheckpointChain for Order {
    async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
        let zero = Fixed::ZERO;
        let (signer, checkpoint) = (AccountId([0; 20]), None);
        Ok(VaultState {
            height: 1,
            signer,
            checkpoint,
            balance: zero,
            deposits: 0,
            deposited: zero,
            distributions_paid: zero,
        })
    }

    async fn observe(&self, view: View, _approvals: &[crate::checkpoints::ApprovalId]) -> anyhow::Result<Observation> {
        let lifecycle = LifecycleEvidence {
            height: 1,
            block_hash: B256::repeat_byte(1),
            signer: AccountId([0; 20]),
            signer_epoch: 1,
            withdrawals_paused: false,
            total_withdrawn: Fixed::ZERO,
            checkpoint_seq: None,
            checkpoint_root: None,
            approvals: Vec::new(),
        };
        Ok(Observation { view, state: self.state(view).await?, lifecycle: Some(lifecycle) })
    }

    async fn post(&self, _checkpoint: &Checkpoint) -> anyhow::Result<()> {
        bail!("this vault takes no checkpoints")
    }

    async fn turn_order(&self, _at: &Observation, turn: u64) -> anyhow::Result<Vec<HandoverSource>> {
        assert_eq!(turn, TURN);
        Ok(self.order.clone())
    }
}

/// The release whose verification A's asker gives the sequencer, the order being A, B, whose replica
/// verified `b_head`, if any, and C, whose replica verified the checkpoint due.
async fn first_admitted(b_head: Option<VerifiedHead>) -> B256 {
    let (b, b_endpoint) = following(0xb, b_head);
    let (c, c_endpoint) = following(0xc, Some(head()));
    let order = vec![member(0xa, "127.0.0.1:1"), member(0xb, &b_endpoint), member(0xc, &c_endpoint)];
    let (sequencer, mut requests) = mpsc::channel(4);
    let (due_tx, due_rx) = watch::channel(Some(due(AccountId([0xa; 20]), sequencers(&[0xa]))));
    let asker = asker(order, &sequencer, due_rx);
    let taken = async {
        let Some(Request::LogChecked { head: checked, candidate, release, reply }) = requests.recv().await else {
            panic!("the asker gave the sequencer nothing");
        };
        assert_eq!(release_of(candidate.0[0]), release, "the verifier's candidate is not its release's");
        due_tx.send_replace(None);
        reply.send(true).unwrap();
        (checked, release)
    };
    // The asker starts once both members' endpoints accept connections.
    let asked = async {
        for endpoint in [&b_endpoint, &c_endpoint] {
            while TcpStream::connect(endpoint.as_str()).await.is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        tokio::select! {
            taken = taken => taken,
            ran = asker.run() => panic!("the asker stopped: {ran:?}"),
        }
    };
    let (checked, release) = tokio::select! {
        taken = asked => taken,
        never = b.run() => match never {},
        never = c.run() => match never {},
    };
    assert_eq!(checked, head(), "the asker gave another head");
    release
}

/// A's asker over the schedule's `order`, signing with `key()`, giving what it admits to `sequencer`.
fn asker(
    order: Vec<HandoverSource>,
    sequencer: &mpsc::Sender<Request>,
    due: watch::Receiver<Option<Due>>,
) -> Asker<Order> {
    let own = Platform { launch_digest: [0; 48], policy: 0, tcb: 0 };
    let trusted = crate::test_support::trusted(Arc::new(crate::clock::test_clock::TestClock::default()));
    Asker::new(Order { order }, sequencer, due, (domain(), TeeProvider::Mock, own), key(), trusted)
}

/// The operator enclave's key, which signs its checkpoints and its questions.
fn key() -> Arc<EnclaveKey> {
    Arc::new(EnclaveKey::test_key())
}

/// The alerts, or their resolutions, named `name` that `lines` has received.
fn named(lines: &mut mpsc::UnboundedReceiver<Alert>, name: &str) -> Vec<Alert> {
    let named = |alert: &Alert| alert.get("alert").or(alert.get("alert_resolved")).is_some_and(|got| got == name);
    std::iter::from_fn(|| lines.try_recv().ok()).filter(named).collect()
}

/// A following member running `byte`'s release whose replica verified `head`, if any, in `TURN` at
/// position 0, as `key()` signed it, and its endpoint.
fn following(byte: u8, head: Option<VerifiedHead>) -> (Answering, String) {
    let checked = CheckedHeads::default();
    if let Some(head) = head {
        checked.record(Checked { head, taken: Some((TURN, 0)), signer: AccountId(key().address()) });
    }
    let mut cfg = dev_config();
    let listen = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    cfg.tee.sealing = Some(toml::from_str(&format!("dir = '/nonexistent'\n[handover]\nlisten = '{listen}'")).unwrap());
    cfg.file_sha256 = CONFIG;
    cfg.custody.chain_id = domain().chain_id;
    cfg.custody.vault = domain().vault.to_string();
    let platform = Platform { launch_digest: [byte; 48], policy: 0, tcb: 0 };
    let answering = Answering::new(&cfg, Box::new(MockAttestor), platform, checked).unwrap().unwrap();
    (answering, listen.to_string())
}

/// `byte`'s member, registering `byte`'s release and `endpoint`.
fn member(byte: u8, endpoint: &str) -> HandoverSource {
    HandoverSource {
        candidate: AccountId([byte; 20]),
        release: release_of(byte).0,
        api_hostname: format!("node-{byte}.test"),
        handover_endpoint: endpoint.into(),
    }
}

/// The release of `byte`'s measurement under `CONFIG`.
fn release_of(byte: u8) -> B256 {
    B256::from(Release { launch_digest: [byte; 48], config_sha256: CONFIG }.id())
}

/// `byte`'s member's answer to `nonce` and `statement`, running `config`.
async fn answer(byte: u8, config: [u8; 32], nonce: &[u8; 32], statement: &Statement) -> Answer {
    let quoting = evidence::Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "https://kds.test" };
    let platform = Platform { launch_digest: [byte; 48], policy: 0, tcb: 0 };
    let bound = report_data(domain(), nonce, statement, &config);
    let evidence = evidence::produce(&quoting, &MockAttestor, &platform, bound).await.unwrap();
    Answer::Verified { config_sha256: hex::encode(config), evidence }
}

/// The check due on `TURN`, asked by `candidate`, with `excluded` sequencing its range.
fn due(candidate: AccountId, excluded: Sequencers) -> Due {
    Due { turn: TURN, position: 0, set: 1, candidate, head: head(), excluded }
}

/// Who sequenced a range: each of `bytes`' members on its own release.
fn sequencers(bytes: &[u8]) -> Sequencers {
    Sequencers {
        candidates: bytes.iter().map(|byte| AccountId([*byte; 20])).collect(),
        releases: bytes.iter().map(|byte| release_of(*byte)).collect(),
    }
}

/// The statement of the check due, under signer epoch 1.
fn statement() -> Statement {
    let boundary = engine_core::turns::boundary(TURN).unwrap();
    Statement { signer_epoch: 1, boundary, turn: TURN, position: 0, set: 1, head: head() }
}

/// The checkpoint the check verifies.
fn head() -> VerifiedHead {
    VerifiedHead { seq: 40, link: B256::repeat_byte(4), root: B256::repeat_byte(0x40) }
}

/// The deployment the members answer in.
fn domain() -> CheckpointDomain {
    CheckpointDomain { chain_id: 31_337, vault: AccountId([0x77; 20]) }
}
