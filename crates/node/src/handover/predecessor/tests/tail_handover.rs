//! The tail handover over loopback: a successor following the predecessor's log takes only the
//! entries past its tip; a forged entry, a wrong seal, a head mismatch and a posted-head mismatch
//! are each refused before the confirmation; a tip the predecessor does not hold, or one it no
//! longer retains, is refused before the stop, saying why, while the predecessor keeps its keys and
//! sequences on; a member whose copy's segments are its own writer's falls back to the whole log
//! once it sets the copy aside; and a successor that reads an older protocol version than the log,
//! following or with no copy, is refused before the stop.

use super::*;

use std::path::Path;

use crate::handover::tail::{Prepared, TailMiss, TailRefused, Taken, Tip, take_tail, version_name};
use crate::log::{MANIFEST, Manifest, offset_of};
use crate::sequencer::Request as SequencerRequest;
use crate::standby::Follower;
use crate::test_support::{SEGMENT_COMMANDS, TestSequencer, log_entries};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

#[tokio::test]
async fn a_following_successor_takes_only_the_entries_after_its_tip() {
    let mut tailed = Tailed::start("tail-moves", 6, 5).await;
    let (tip, _) = tailed.follower.tip().unwrap();
    let copied = log_entries(&tailed.exchange.successor_log).len();
    let stopped = tailed.answer_stop(|_| {}, None);
    let posted = tailed.posted_at(tip + 2);
    let taken = tailed.take(&tailed.prepared(Some(posted))).await.unwrap();
    let head = stopped.await.unwrap();

    let Taken { store, after, entries, bytes } = taken;
    assert_eq!(after, tip, "the tail does not follow the copy's tip");
    let operator = log_entries(&tailed.operator);
    assert_eq!(entries as usize, operator.len() - copied, "the copy did not take exactly the entries it lacked");
    assert_eq!(bytes, lines_after(&tailed.operator, tip).len() as u64, "more than the entries after the tip moved");
    drop(tailed.follower);
    assert_eq!(log_entries(&tailed.exchange.successor_log), operator, "the copy is not the predecessor's log");
    assert_eq!(store.keys().predecessor_head.map(|head| head.seq), Some(head));
    assert_eq!((store.release(), store.generation()), (tailed.exchange.upgrade(), GENERATION));
    timeout(Duration::from_secs(5), tailed.exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
}

/// A successor whose attempt fails after the stop retries the same tip and still gets the tail: with
/// the sequencer gone, the predecessor finds the tip on its final log from the last tip it found
/// (`prefix_on_disk`), and checks the versions read against the one the stop retained.
#[tokio::test]
async fn a_retry_after_the_stop_finds_the_tip_on_the_stopped_log() {
    let mut tailed = Tailed::start("tail-retry", 6, 5).await;
    let (tip, _) = tailed.follower.tip().unwrap();
    let stopped = tailed.answer_stop(|_| {}, None);
    let unregistered = Prepared { signer: AccountId([9; 20]), ..tailed.prepared(None) };
    let err = tailed.take(&unregistered).await.err().expect("keys for a signer the vault does not name");
    assert!(err.to_string().contains("is not the vault's signer"), "{err:#}");
    let head = stopped.await.unwrap();
    tailed.assert_unconfirmed();

    let taken = tailed.take(&tailed.prepared(None)).await.unwrap();
    let Taken { store, after, bytes, .. } = taken;
    assert_eq!((after, bytes), (tip, lines_after(&tailed.operator, tip).len() as u64));
    assert_eq!(store.keys().predecessor_head.map(|head| head.seq), Some(head));
    timeout(Duration::from_secs(5), tailed.exchange.run).await.expect("the predecessor ended").unwrap().unwrap();
}

#[tokio::test]
async fn a_forged_entry_in_the_tail_is_refused_before_the_confirmation() {
    let mut tailed = Tailed::start("tail-forged", 6, 5).await;
    let stopped = tailed.answer_stop(|log| rewrite_last_command(log, |command| command.sequenced.ts += 1), None);
    let err = tailed.take(&tailed.prepared(None)).await.err().expect("a forged entry was taken");
    stopped.await.unwrap();
    assert!(format!("{err:#}").contains("does not extend the copy"), "{err:#}");
    tailed.assert_unconfirmed();
}

#[tokio::test]
async fn a_wrong_seal_in_the_tail_is_refused_before_the_confirmation() {
    let mut tailed = Tailed::start("tail-wrong-seal", 6, 5).await;
    let reseal = |command: &mut crate::log::CommandEntry| {
        command.seal = Some(EnclaveKey::generate().seal_command(DOMAIN, engine_core::eip712::B256::ZERO));
    };
    let stopped = tailed.answer_stop(move |log| rewrite_last_command(log, reseal), None);
    let err = tailed.take(&tailed.prepared(None)).await.err().expect("a wrongly sealed entry was taken");
    stopped.await.unwrap();
    assert!(format!("{err:#}").contains("sealed by"), "{err:#}");
    tailed.assert_unconfirmed();
}

#[tokio::test]
async fn a_head_the_tail_does_not_reach_is_refused_before_the_confirmation() {
    let mut tailed = Tailed::start("tail-head", 6, 5).await;
    let stopped = tailed.answer_stop(|_| {}, Some(format!("0x{}", "ab".repeat(32))));
    let err = tailed.take(&tailed.prepared(None)).await.err().expect("a tail missing the head was taken");
    stopped.await.unwrap();
    assert!(format!("{err:#}").contains("does not end at the predecessor's final checkpoint"), "{err:#}");
    tailed.assert_unconfirmed();
}

#[tokio::test]
async fn a_copy_without_the_latest_posted_head_is_refused_before_the_confirmation() {
    let mut tailed = Tailed::start("tail-posted", 6, 5).await;
    let (tip, _) = tailed.follower.tip().unwrap();
    let stopped = tailed.answer_stop(|_| {}, None);
    let posted = crate::log_heads::Head { seq: tip + 1, link: engine_core::eip712::B256::repeat_byte(9) };
    let err = tailed.take(&tailed.prepared(Some(posted))).await.err().expect("a foreign posted head passed");
    stopped.await.unwrap();
    assert!(format!("{err:#}").contains("does not hold the latest posted log head"), "{err:#}");
    tailed.assert_unconfirmed();
}

/// A follower further behind than the predecessor retains prefixes for is refused before the stop as
/// lagging: the predecessor keeps its keys and sequences on, and the copy stays as it was.
#[tokio::test]
async fn a_lagging_tip_is_refused_before_the_stop_and_the_predecessor_sequences_on() {
    let operator = scratch_dir("tail-lagged-operator");
    let early = scratch_dir("tail-lagged-early");
    let node = TestSequencer::start(&operator, 1024);
    ticks(&node, 6).await;
    copy_files(&operator, &early);
    // Past what a restarted sequencer retains, so the copy's tip is older than the log retains.
    ticks(&node, crate::sequencer::prefixes::RETAINED_PREFIXES + 64).await;
    node.stop().await;
    let node = TestSequencer::start(&operator, 1024);
    let mut tailed = Tailed::over("tail-lagged", operator, node, &early).await;
    let tip = tailed.follower.tip().unwrap();
    let err = tailed.take(&tailed.prepared(None)).await.err().expect("a lagging tip took the tail");
    let refused = err.downcast_ref::<TailRefused>().expect("a typed refusal");
    assert_eq!(refused.miss, TailMiss::Lagged, "{err:#}");
    tailed.assert_sequencing_on().await;
    assert_eq!(tailed.follower.tip(), Some(tip), "a refused tail changed the copy");
}

/// A tip naming another history than the log holds at its seq, or a seq past its head, is refused
/// before the stop as not on the log, and the predecessor sequences on.
#[tokio::test]
async fn a_tip_not_on_the_log_is_refused_before_the_stop() {
    let mut tailed = Tailed::start("tail-not-on-log", 6, 0).await;
    let (seq, link) = tailed.follower.tip().unwrap();
    let latest = version_name(ProtocolVersion::LATEST);
    for tip in [Tip { seq, link: engine_core::eip712::B256::repeat_byte(7) }, Tip { seq: seq + 1_000, link }] {
        let (mut stream, request) = tailed.exchange.connect_tail(Some(&tip), &latest).await;
        send(&mut stream, &request).await.unwrap();
        let reply = timeout(Duration::from_secs(5), receive(&mut stream)).await.expect("no answer in time").unwrap();
        let Reply::Missed { miss, .. } = reply else { panic!("keys for a tip not on the log") };
        assert_eq!(miss, TailMiss::NotOnLog, "{tip:?}");
        tailed.assert_sequencing_on().await;
    }
}

/// A member whose copy's segments are its own writer's, cut after an earlier stop-seal where the
/// holder's were not, falls back to the whole log: it sets the copy aside and takes the holder's own
/// segments, as a node with no copy does, whatever boundaries the copy had.
#[tokio::test]
async fn a_member_falls_back_to_the_whole_log_whatever_its_copys_segments() {
    let Tailed { exchange, operator, node, follower } = Tailed::start("tail-aside", 6, 5).await;
    let (tip, _) = follower.tip().unwrap();
    drop(follower);
    // The copy's own writer cuts segments of another length than the holder's.
    let signer = signer();
    let mut follower =
        Follower::open(&exchange.successor_log, 3, DOMAIN, signer, [signer].into(), None, |_| {}).unwrap();
    follower.take_tail(tip, &lines_after(&operator, tip)).unwrap();
    drop(follower);

    crate::node::member::turn::set_copy_aside(&exchange.successor_log).unwrap();
    let unused = scratch_dir("tail-aside-unused");
    let follower = Follower::open(&unused, 3, DOMAIN, signer, [signer].into(), None, |_| {}).unwrap();
    let mut tailed = Tailed { exchange, operator, node: Some(node.expect("the node runs")), follower };
    let stopped = tailed.answer_stop(|_| {}, None);
    let store = timeout(Duration::from_secs(30), tailed.exchange.take()).await.expect("the take hung").unwrap();
    let head = stopped.await.unwrap();
    assert_eq!(store.keys().predecessor_head.map(|head| head.seq), Some(head));
    assert_eq!(log_entries(&tailed.exchange.successor_log), log_entries(&tailed.operator), "the log taken");
    let ends =
        |dir: &Path| Manifest::load(dir).unwrap().segments.iter().map(|listed| listed.last_seq).collect::<Vec<_>>();
    let aside = tailed.exchange.successor_log.with_extension("aside");
    assert_ne!(ends(&aside), ends(&tailed.operator), "the copy's segments were the holder's");
}

/// The tail's size is judged before the stop, from the log's listings and its open segment: the
/// estimate covers every byte the tail would carry, so a bound below them sends whole segments.
#[tokio::test]
async fn a_tail_over_its_bound_is_judged_before_the_stop() {
    let tailed = Tailed::start("tail-bound", 6, 5).await;
    let (tip, _) = tailed.follower.tip().unwrap();
    let carried = lines_after(&tailed.operator, tip).len() as u64;
    assert!(super::super::checks::tail_over(&tailed.operator, tip, carried - 1), "a tail over its bound fits");
    assert!(!super::super::checks::tail_over(&tailed.operator, tip, 1 << 30), "a small tail is over the bound");
}

/// A successor whose build reads only older protocol versions than the log runs could not replay
/// it: the predecessor refuses before it stops sequencing.
#[tokio::test]
async fn a_successor_reading_an_older_protocol_version_is_refused_before_the_stop() {
    let mut tailed = Tailed::start("tail-version", 6, 0).await;
    let (seq, link) = tailed.follower.tip().unwrap();
    tailed.refuses_then_hands_over(Some(&Tip { seq, link })).await;
}

/// A successor with no copy, such as a fresh elected node or a `--predecessor` start, takes whole
/// segments, and is held to the same rule: one whose build reads only older protocol versions than
/// the log runs is refused before the stop, and the predecessor keeps its keys and sequences on.
#[tokio::test]
async fn a_successor_with_no_copy_reading_an_older_protocol_version_is_refused_before_the_stop() {
    let mut tailed = Tailed::start("whole-version", 6, 0).await;
    tailed.refuses_then_hands_over(None).await;
}

// ── Helpers ──

use engine_core::protocol::ProtocolVersion;

/// A predecessor whose node is the test sequencer on its log, and a successor following that log.
struct Tailed {
    exchange: Exchange,
    operator: PathBuf,
    node: Option<TestSequencer>,
    follower: Follower,
}

impl Tailed {
    /// A request for the keys, at `tip` or with no copy, from a build that reads only `V50` is
    /// refused while the predecessor's node sequences on; the same request from this build gets the
    /// keys.
    async fn refuses_then_hands_over(&mut self, tip: Option<&Tip>) {
        let (mut stream, request) = self.exchange.connect_tail(tip, &version_name(ProtocolVersion::V50)).await;
        send(&mut stream, &request).await.unwrap();
        let reply = timeout(Duration::from_secs(5), receive(&mut stream)).await.expect("no answer in time").unwrap();
        let Reply::Refused { reason } = reply else { panic!("keys for a successor that cannot read the log") };
        assert!(reason.contains("newer than v50"), "{reason}");
        assert!(self.exchange.drains.try_recv().is_err(), "sequencing stopped");
        assert!(!self.exchange.run.is_finished(), "the predecessor stopped serving");
        ticks(self.node.as_ref().expect("the node runs"), 1).await;

        let (mut stream, request) = self.exchange.connect_tail(tip, &version_name(ProtocolVersion::LATEST)).await;
        let _stopped = self.answer_stop(|_| {}, None);
        send(&mut stream, &request).await.unwrap();
        let reply = timeout(Duration::from_secs(5), receive(&mut stream)).await.expect("no answer in time").unwrap();
        assert!(matches!(reply, Reply::Keys { .. }), "a successor reading the log's version got no keys");
    }

    /// The predecessor's node sequences `before` ticks; the successor copies its log and builds its
    /// replica from the copy; the node then sequences `after` more.
    async fn start(name: &str, before: usize, after: usize) -> Self {
        let operator = scratch_dir(&format!("{name}-operator"));
        let early = scratch_dir(&format!("{name}-early"));
        let node = TestSequencer::start(&operator, 1024);
        ticks(&node, before).await;
        copy_files(&operator, &early);
        ticks(&node, after).await;
        Self::over(name, operator, node, &early).await
    }

    /// A predecessor over `node`, running on the log in `operator`, and a successor whose copy is
    /// the log in `copy`.
    async fn over(name: &str, operator: PathBuf, node: TestSequencer, copy: &Path) -> Self {
        let exchange = Exchange::start_with(name, Some(node.tx.clone()), Some(operator.clone())).await;
        exchange.make_upgrade_current();
        copy_files(copy, &exchange.successor_log);
        let follower = Follower::open(
            &exchange.successor_log,
            SEGMENT_COMMANDS,
            DOMAIN,
            signer(),
            [signer()].into(),
            None,
            |_| {},
        )
        .unwrap();
        Self { exchange, operator, node: Some(node), follower }
    }

    /// Answers the predecessor's stop as the node does — a final checkpoint through the sequencer,
    /// which seals the log's tail segment, then the drained head — after `alter` changes the log on
    /// disk, as the host may, and with the head's root replaced by `root`, when given. The task ends
    /// with the head's seq.
    fn answer_stop(&mut self, alter: impl FnOnce(&Path) + Send + 'static, root: Option<String>) -> JoinHandle<u64> {
        let node = self.node.take().expect("the node runs");
        let mut drains = std::mem::replace(&mut self.exchange.drains, mpsc::channel(1).1);
        let operator = self.operator.clone();
        tokio::spawn(async move {
            let reply = drains.recv().await.expect("the predecessor asks to stop");
            let (stop, stopped) = oneshot::channel();
            node.tx.send(SequencerRequest::FinalCheckpoint(stop)).await.unwrap();
            stopped.await.unwrap().unwrap();
            node.stop().await;
            alter(&operator);
            let Some(LogEntry::Checkpoint(last)) = log_entries(&operator).pop() else {
                panic!("the drained log ends at its final checkpoint")
            };
            let head = Head { seq: last.seq, root: root.unwrap_or(last.root) };
            let _ = reply.send(Ok(head));
            last.seq
        })
    }

    /// What the successor reads before connecting: the vault naming the old key, and `posted`.
    fn prepared(&self, posted: Option<crate::log_heads::Head>) -> Prepared {
        let signer = AccountId(self.exchange.old.enclave_key().unwrap().address());
        Prepared { generation: GENERATION, signer, posted }
    }

    /// The head the predecessor's log holds at `seq`, as it would post it.
    fn posted_at(&self, seq: u64) -> crate::log_heads::Head {
        let replayed = crate::sequencer::replay_path(&self.operator).unwrap();
        crate::log_heads::Head { seq, link: replayed.prefix_ring.prefix_at(seq).expect("the log holds the seq") }
    }

    /// The successor's tail handover.
    async fn take(&mut self, prepared: &Prepared) -> anyhow::Result<Taken> {
        let sealer = MockSealer { launch_digest: UPGRADE };
        take_tail(
            &self.exchange.successor,
            &self.exchange.address,
            &sealer,
            &MockAttestor,
            prepared,
            &mut self.follower,
        )
        .await
    }

    /// The predecessor was not asked to stop, goes on serving, and its node sequences on.
    async fn assert_sequencing_on(&mut self) {
        assert!(self.exchange.drains.try_recv().is_err(), "sequencing stopped");
        assert!(!self.exchange.run.is_finished(), "the predecessor stopped serving");
        ticks(self.node.as_ref().expect("the node runs"), 1).await;
    }

    /// Nothing was sealed and the predecessor goes on serving: it saw no confirmation.
    fn assert_unconfirmed(&self) {
        assert!(!file_for(&self.exchange.successor_dir, &self.exchange.upgrade(), GENERATION).exists(), "keys sealed");
        assert!(!self.exchange.run.is_finished(), "the predecessor ended without a confirmation");
    }
}

impl Exchange {
    /// Connects as a successor running `UPGRADE` whose build reads up to `reads` and builds a request
    /// for the keys, a tail request at `tip` when given: the stream and the request.
    async fn connect_tail(&self, tip: Option<&Tip>, reads: &str) -> (TcpStream, Request) {
        let mut stream = TcpStream::connect(&self.address).await.unwrap();
        let challenge: Challenge = receive(&mut stream).await.unwrap();
        let challenge: [u8; 32] = hex::decode(challenge.nonce).unwrap().try_into().unwrap();
        let (_, public) = <Kx as hpke::Kem>::gen_keypair();
        let public = hpke::Serializable::to_bytes(&public).to_vec();
        let config = self.successor.file_sha256;
        let report_data = match tip {
            Some(tip) => {
                crate::handover::tail::tail_report_data(self.domain(), &challenge, &public, &config, reads, tip)
            }
            None => request_report_data(self.domain(), &challenge, &public, &config, reads),
        };
        let platform = Platform { launch_digest: UPGRADE, policy: 0, tcb: 0 };
        let quoting = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "https://kds.test" };
        let evidence = evidence::produce(&quoting, &MockAttestor, &platform, report_data).await.unwrap();
        let request = Request {
            public_key: hex::encode(&public),
            config_sha256: hex::encode(config),
            log_tip: None,
            tail: tip.cloned(),
            reads: reads.into(),
            secrets_only: false,
            evidence,
        };
        (stream, request)
    }
}

/// The test sequencer's enclave, which seals and signs its log.
fn signer() -> AccountId {
    AccountId(EnclaveKey::test_key().address())
}

/// Sequences `count` ticks on `node`.
async fn ticks(node: &TestSequencer, count: usize) {
    for _ in 0..count {
        node.submit(Command::Tick).await;
    }
}

/// Copies the log files in `from` into `to`, as a follower's copy of them.
fn copy_files(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for file in std::fs::read_dir(from).unwrap().map(Result::unwrap) {
        if file.file_type().unwrap().is_file() {
            std::fs::copy(file.path(), to.join(file.file_name())).unwrap();
        }
    }
}

/// The JSON lines the log in `dir` holds after the command at `after`.
fn lines_after(dir: &Path, after: u64) -> Vec<u8> {
    let mut previous = None;
    let mut bytes = Vec::new();
    for entry in log_entries(dir) {
        let offset = offset_of(previous, &entry).unwrap();
        previous = Some(offset);
        if offset > crate::log::Offset::start_of(after) {
            bytes.extend(serde_json::to_vec(&entry).unwrap());
            bytes.push(b'\n');
        }
    }
    bytes
}

/// Changes the last command of the log in `dir` with `change` and relists its segment, as a host
/// that rewrites the log consistently does.
fn rewrite_last_command(dir: &Path, change: impl FnOnce(&mut crate::log::CommandEntry)) {
    let manifest = Manifest::load(dir).unwrap();
    let listed = manifest.segments.last().expect("the drain listed the final segment").clone();
    let text = std::fs::read_to_string(dir.join(&listed.file)).unwrap();
    let last = text
        .lines()
        .filter_map(|line| match serde_json::from_str(line).unwrap() {
            LogEntry::Command(command) => Some(command.sequenced.seq),
            _ => None,
        })
        .next_back()
        .expect("the final segment holds a command");
    let mut change = Some(change);
    let mut rewritten = String::new();
    for line in text.lines() {
        let mut entry: LogEntry = serde_json::from_str(line).unwrap();
        if let LogEntry::Command(command) = &mut entry
            && command.sequenced.seq == last
        {
            (change.take().unwrap())(command);
        }
        rewritten.push_str(&serde_json::to_string(&entry).unwrap());
        rewritten.push('\n');
    }
    std::fs::write(dir.join(&listed.file), &rewritten).unwrap();
    let relisted = crate::log::SegmentInfo {
        bytes: rewritten.len() as u64,
        sha256: hex::encode(sha2::Sha256::digest(rewritten.as_bytes())),
        ..listed.clone()
    };
    let manifest = std::fs::read_to_string(dir.join(MANIFEST))
        .unwrap()
        .replace(&serde_json::to_string(&listed).unwrap(), &serde_json::to_string(&relisted).unwrap());
    std::fs::write(dir.join(MANIFEST), manifest).unwrap();
}
