//! The worker's steps on one thread along drawn schedules, against an inline engine replaying the
//! same commands: every commit's root is the inline root at its seq, every read answers from the
//! newest released version exactly as the inline engine did at that version, no read runs while a
//! commit is waited on, and payout proofs are the inline engine's at the checkpoint they name.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc::sync_channel;

use engine_core::Engine;
use engine_core::command::{AdminCommand, Command};
use engine_core::eip712::B256;
use engine_core::merkle::{AccountProof, ExitProof, WithdrawalApprovalProof};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use tokio::sync::oneshot;

use super::*;
use crate::common::{BTC, Client, Harness, acct, f};
use crate::root_worker::{Message, Read, Work};

#[test]
fn a_drawn_schedule_commits_the_inline_roots_and_reads_only_released_versions() {
    let op = prop_oneof![
        4 => (0..5u8, 1..4u8).prop_map(|(kind, account)| Op::Command(kind, account)),
        3 => (1..64usize).prop_map(Op::Part),
        3 => (1..16usize).prop_map(Op::Steps),
        2 => Just(Op::Commit),
        2 => Just(Op::Release),
        2 => (1..4u8).prop_map(Op::Read),
        1 => Just(Op::Prove),
    ];
    let mut runner = TestRunner::new(Config {
        cases: 48,
        rng_seed: RngSeed::Fixed(0x925_0003),
        failure_persistence: None,
        ..Config::default()
    });
    let covered = RefCell::new(Coverage::default());
    let result = runner.run(&vec(op, 1..80), |ops| {
        let covered = &mut covered.borrow_mut();
        let mut run = Run::new();
        for op in ops {
            run.take(op, covered).map_err(TestCaseError::fail)?;
        }
        run.take(Op::Commit, covered).map_err(TestCaseError::fail)?;
        run.take(Op::Release, covered).map_err(TestCaseError::fail)?;
        (1..4).try_for_each(|account| run.take(Op::Read(account), covered)).map_err(TestCaseError::fail)
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
    let covered = covered.into_inner();
    assert!(covered.commits > 100 && covered.proofs > 50 && covered.multi_step_exits > 10, "{covered:?}");
    assert!(covered.approvals_proved > 10, "{covered:?}");
}

#[test]
fn after_a_commit_a_queued_read_and_payout_proof_run_with_nothing_more_sent() {
    let mut run = Run::new();
    run.take(Op::Release, &mut Coverage::default()).unwrap();
    run.command(1, trader(2));
    let seq = run.h.engine.last_seq().expect("commands applied");
    let (proved_at, approvals) = {
        let (&at, proven) = run.proven.iter().next_back().expect("the first root is proven");
        (at, proven.approvals.keys().copied().collect::<Vec<_>>())
    };
    assert!(!approvals.is_empty(), "the book holds approvals to prove");
    // A payout proof and a read wait behind the commit the sequencer is waiting on.
    run.state.take(Message::Work(Work::Prove(proved_at, approvals)));
    let (reply, mut answer) = oneshot::channel();
    run.state.take(Message::Read(Read::Account(trader(1), reply)));
    let (commit_reply, committed) = sync_channel(1);
    run.progress.set_commit_waiting(true);
    run.state.take(Message::Work(Work::Commit(run.h.engine.commit_inputs(), commit_reply)));
    while committed.try_recv().is_err() {
        run.state.step(&run.progress).expect("the commit is answered");
    }
    assert!(answer.try_recv().is_err(), "the read waited for the commit");
    // The sequencer has not cleared the flag yet, and sends nothing more: the worker goes on alone.
    while run.state.step(&run.progress).is_some() {}
    assert!(matches!(answer.try_recv(), Ok(Some(_))), "the read ran after the commit, against the released root");
    let proved = run.state.take_outbox();
    assert!(matches!(proved.as_slice(), [Outcome::Proved { seq: at, .. }] if *at == proved_at), "{proved:?}");
    assert!(seq > proved_at);
}

#[test]
fn writes_with_no_release_leave_no_garbage_once_the_worker_is_idle() {
    let mut run = Run::new();
    run.take(Op::Release, &mut Coverage::default()).unwrap();
    // Trades rewrite the same accounts' leaves part after part, each replacing a leaf only the working
    // version holds.
    for round in 0..40u8 {
        run.command(1 + round % 2, trader(2 + round % 2));
        run.take(Op::Part(64), &mut Coverage::default()).unwrap();
        run.until_idle_work();
    }
    let written = run.state.tree.nodes();
    while run.state.step(&run.progress).is_some() {}
    assert_eq!(run.state.tree.garbage(), 0, "every dead node is freed with no release");
    assert!(run.state.tree.nodes() < written, "the parts left nodes to free: {written:?}");
}

#[test]
fn a_discard_answers_a_pinned_exit_read_with_none_and_returns_a_payout_proofs_rest_unproved() {
    let mut run = Run::new();
    run.take(Op::Release, &mut Coverage::default()).unwrap();
    let (&seq, proven) = run.proven.iter().next_back().expect("the first root is proven");
    let approvals: Vec<(AccountId, u64)> = proven.approvals.keys().copied().collect();
    assert!(approvals.len() > PROOF_STEP, "the proof job takes more than one step");
    // A payout proof one step in, and an exit read past its first step, both pinned.
    run.state.take(Message::Work(Work::Prove(seq, approvals.clone())));
    let (reply, mut exit) = oneshot::channel();
    run.state.take(Message::Read(Read::Exit(trader(1), reply)));
    while !(matches!(run.state.reads.front(), Some(Reading::Exit(_)))
        && run.state.background.iter().any(|job| matches!(job, Background::Prove { proofs, .. } if !proofs.is_empty())))
    {
        run.state.step(&run.progress).expect("the read and the proof are under way");
    }
    run.state.take(Message::Work(Work::Discard));
    while run.state.step(&run.progress).is_some() {}
    assert!(matches!(exit.try_recv(), Ok(None)), "a discard answers a read under way with none");
    let outbox = run.state.take_outbox();
    let [Outcome::Proved { seq: proved, proofs }] = outbox.as_slice() else { panic!("{outbox:?}") };
    assert_eq!(*proved, seq);
    assert_eq!(proofs.iter().map(|(key, _)| *key).collect::<Vec<_>>(), approvals, "every approval is answered");
    let unproved = proofs.iter().filter(|(_, proof)| proof.is_none()).count();
    assert!(unproved > 0 && unproved < approvals.len(), "the step taken is kept, the rest unproved: {unproved}");
    // The worker goes on from an empty tree.
    let (reply, mut account) = oneshot::channel();
    run.state.take(Message::Read(Read::Account(trader(1), reply)));
    while run.state.step(&run.progress).is_some() {}
    assert!(matches!(account.try_recv(), Ok(None)), "no version is left to read");
}

// ── Helpers ──

#[derive(Clone, Debug)]
enum Op {
    /// A command: deposit, buy, sell, price move or withdrawal, by account.
    Command(u8, u8),
    /// Leaf writes copied out of the engine and sent.
    Part(usize),
    /// Worker steps.
    Steps(usize),
    Commit,
    /// The oldest unreleased version's checkpoint is durable.
    Release,
    /// An account and an exit read.
    Read(u8),
    /// Payout proofs of every approval at the newest committed version.
    Prove,
}

/// The schedules' accounts: the first withdraws, unsigned as `V22` takes withdrawals, and the others
/// trade, as clients whose orders the harness signs.
fn trader(n: u8) -> AccountId {
    if n == 1 { acct(1) } else { Client::new(n).account }
}

/// What the schedules reached.
#[derive(Debug, Default)]
struct Coverage {
    commits: usize,
    proofs: usize,
    multi_step_exits: usize,
    approvals_proved: usize,
}

/// What the inline engine proves at one root.
struct Proven {
    root: B256,
    accounts: BTreeMap<AccountId, Option<AccountProof>>,
    exits: BTreeMap<AccountId, Option<ExitProof>>,
    approvals: BTreeMap<(AccountId, u64), Option<WithdrawalApprovalProof>>,
}

/// One schedule's sequencer, worker and inline reference.
struct Run {
    h: Harness,
    inline: Engine,
    replayed: usize,
    state: WorkerState,
    progress: Progress,
    unreleased: VecDeque<(u64, B256)>,
    released: Option<u64>,
    proven: BTreeMap<u64, Proven>,
    price: i64,
}

impl Run {
    /// A book under `V38`, the exit's version, whose client actions need no screening, with three
    /// funded accounts and one with many withdrawals, its tree detached at its first root. The leaves
    /// later versions add are the core's `root_deltas` test's.
    fn new() -> Self {
        // Deposits with no payer are credited under `V22`, and held from the version that needs one,
        // which the schedules' deposits are, so held deposits' leaves change too.
        let mut h = Harness::zero_house_operated(ProtocolVersion::V22);
        // Accounts whose orders and withdrawals the harness signs.
        (2..4).for_each(|seed| _ = h.signer(seed));
        h.price(BTC, "100");
        for account in 1..4 {
            h.deposit(trader(account), "100000");
        }
        for _ in 0..40 {
            h.cmd(Command::Withdraw { account: trader(1), amount: f("1"), auth: None });
        }
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V38 });
        let mut run = Self {
            inline: Engine::new(),
            replayed: 0,
            state: WorkerState::new(StateTree::default()),
            progress: Progress::default(),
            unreleased: VecDeque::new(),
            released: None,
            proven: BTreeMap::new(),
            price: 100,
            h,
        };
        run.h.engine.state_root();
        let tree = run.h.engine.detach_tree().expect("a root under V9 leaves a tree");
        let (seq, root) = tree.newest().map(|v| (v.seq(), v.root())).expect("committed");
        run.state.take(Message::Work(Work::Install(Box::new(tree))));
        run.prove_inline(seq, root);
        run.unreleased.push_back((seq, root));
        run
    }

    fn take(&mut self, op: Op, covered: &mut Coverage) -> Result<(), String> {
        match op {
            Op::Command(kind, account) => self.command(kind, trader(account)),
            Op::Part(entries) => {
                let delta = self.h.engine.take_root_delta(entries);
                let units = delta.len() as u64;
                self.state.take(Message::Work(Work::Advance(delta, units)));
            }
            Op::Steps(steps) => (0..steps).for_each(|_| _ = self.state.step(&self.progress)),
            Op::Commit => self.commit(covered)?,
            Op::Release => {
                let Some((seq, root)) = self.unreleased.pop_front() else { return Ok(()) };
                self.state.take(Message::Work(Work::Release(seq, root)));
                self.until_idle_work();
                self.released = Some(seq);
            }
            Op::Read(account) => self.read(trader(account), covered)?,
            Op::Prove => self.prove(covered)?,
        }
        Ok(())
    }

    /// Applies a command to the engine whose tree the worker holds.
    fn command(&mut self, kind: u8, account: AccountId) {
        match kind {
            0 => {
                _ = self.h.cmd(Command::Deposit {
                    account,
                    amount: f("500"),
                    chain_ref: format!("0x{}", self.h.log.len()),
                })
            }
            1 | 2 => {
                let side = if kind == 1 { Side::Buy } else { Side::Sell };
                _ = self.h.market_order(account, BTC, side, "1", &self.price.to_string());
            }
            3 => {
                self.price += 1;
                _ = self.h.price(BTC, &self.price.to_string());
            }
            _ => _ = self.h.cmd(Command::Withdraw { account, amount: f("2"), auth: None }),
        }
        assert_eq!(self.h.engine.take_tree_event(), None, "no command here changes the protocol version");
    }

    /// Sends the rest of a root's writes and steps the worker until it answers, with the commit
    /// waited on: no read may run meanwhile.
    fn commit(&mut self, covered: &mut Coverage) -> Result<(), String> {
        let seq = self.h.engine.last_seq().expect("commands applied");
        let (reply, answer) = sync_channel(1);
        self.progress.set_commit_waiting(true);
        // A read forwarded once the commit is waited on goes behind it.
        let (read_reply, mut read) = oneshot::channel();
        self.state.take(Message::Read(Read::Account(trader(1), read_reply)));
        self.state.take(Message::Work(Work::Commit(self.h.engine.commit_inputs(), reply)));
        let (root, _) = loop {
            if let Ok(answer) = answer.try_recv() {
                break answer;
            }
            self.state.step(&self.progress).ok_or("the worker stalled before answering a commit")?;
        };
        if read.try_recv().is_ok() {
            return Err("a read ran while a commit was waited on".into());
        }
        self.progress.set_commit_waiting(false);
        let inline = self.prove_inline(seq, root);
        if inline != root {
            return Err(format!("seq {seq}: the worker committed {root}, the inline engine {inline}"));
        }
        // The read then answers from the newest released version, not the commit's.
        let proof = loop {
            if let Ok(proof) = read.try_recv() {
                break proof;
            }
            self.state.step(&self.progress).ok_or("the worker stalled before answering a read")?;
        };
        let want = self.released.and_then(|at| self.proven[&at].accounts.get(&trader(1)).cloned().flatten());
        if proof != want {
            return Err(format!("seq {seq}: a read behind a commit answered other than the released version"));
        }
        self.unreleased.push_back((seq, root));
        covered.commits += 1;
        Ok(())
    }

    /// Replays the commands since the last root on the inline engine, takes its root, and records
    /// every proof it gives there.
    fn prove_inline(&mut self, seq: u64, root: B256) -> B256 {
        for sequenced in &self.h.log[self.replayed..] {
            self.inline.apply(sequenced).expect("in sequence");
        }
        self.replayed = self.h.log.len();
        let inline = B256::from(self.inline.state_root());
        let accounts: Vec<AccountId> = self.inline.accounts().keys().copied().collect();
        let approvals = self.inline.withdrawal_approvals().map(|a| (a.account, a.nonce)).collect::<Vec<_>>();
        self.proven.insert(
            seq,
            Proven {
                root,
                accounts: accounts.iter().map(|a| (*a, self.inline.account_proof(*a))).collect(),
                exits: accounts.iter().map(|a| (*a, self.inline.exit_proof(*a))).collect(),
                approvals: approvals.into_iter().map(|(a, n)| ((a, n), self.inline.approval_proof(a, n))).collect(),
            },
        );
        inline
    }

    /// Reads `account`'s proofs once the work queued so far is done, so the newest released version
    /// is known, and checks them against the inline engine's at that version. The next release arrives
    /// once the exit read has taken its first step, and moves the newest released version on while
    /// the read, pinned to its own, takes its approvals a step at a time.
    fn read(&mut self, account: AccountId, covered: &mut Coverage) -> Result<(), String> {
        self.until_idle_work();
        let at = self.released.map(|seq| &self.proven[&seq]);
        let (want_proof, want_exit, want_root) = (
            at.and_then(|at| at.accounts.get(&account).cloned().flatten()),
            at.and_then(|at| at.exits.get(&account).cloned().flatten()),
            at.map(|at| at.root),
        );
        let (reply, mut answer) = oneshot::channel();
        self.state.take(Message::Read(Read::Account(account, reply)));
        let proof = loop {
            if let Ok(proof) = answer.try_recv() {
                break proof;
            }
            self.state.step(&self.progress).ok_or("the worker stalled before answering a read")?;
        };
        let (reply, mut answer) = oneshot::channel();
        self.state.take(Message::Read(Read::Exit(account, reply)));
        // The read's first step pins its version; the release arrives after it.
        let mut steps = 0;
        while matches!(self.state.reads.front(), Some(Reading::Read(_))) {
            self.state.step(&self.progress).ok_or("the worker stalled before starting a read")?;
            steps += 1;
        }
        let next = self.unreleased.pop_front();
        if let Some((seq, root)) = next {
            self.state.take(Message::Work(Work::Release(seq, root)));
        }
        let exit = loop {
            if let Ok(exit) = answer.try_recv() {
                break exit;
            }
            self.state.step(&self.progress).ok_or("the worker stalled before answering a read")?;
            steps += 1;
        };
        self.until_idle_work();
        if let Some((seq, _)) = next {
            self.released = Some(seq);
        }
        if steps > 2 {
            covered.multi_step_exits += 1;
        }
        if proof != want_proof || exit != want_exit {
            let seqs = |proof: Option<u64>, exit: Option<u64>| format!("account at {proof:?}, exit at {exit:?}");
            return Err(format!(
                "{account}: a read answered {} where the newest released version at its start gives {}",
                seqs(proof.as_ref().map(|p| p.seq), exit.as_ref().map(|e| e.seq)),
                seqs(want_proof.as_ref().map(|p| p.seq), want_exit.as_ref().map(|e| e.seq)),
            ));
        }
        if let Some(proof) = &proof {
            covered.proofs += 1;
            if Some(proof.root) != want_root {
                return Err("a read answered from an unreleased version".into());
            }
        }
        Ok(())
    }

    /// Asks for every approval's proof at the newest committed version and checks the answer.
    fn prove(&mut self, covered: &mut Coverage) -> Result<(), String> {
        let Some(&seq) = self.proven.keys().next_back() else { return Ok(()) };
        let approvals: Vec<(AccountId, u64)> = self.proven[&seq].approvals.keys().copied().collect();
        self.state.take(Message::Work(Work::Prove(seq, approvals)));
        let proofs = loop {
            if let Some(Outcome::Proved { seq: proved, proofs }) = self.state.take_outbox().into_iter().next() {
                assert_eq!(proved, seq);
                break proofs;
            }
            self.state.step(&self.progress).ok_or("the worker stalled before proving")?;
        };
        for (key, proof) in proofs {
            if Some(&proof) != self.proven[&seq].approvals.get(&key) {
                return Err(format!("{key:?}: a payout proof differs from the inline engine's at seq {seq}"));
            }
            covered.approvals_proved += usize::from(proof.is_some());
        }
        Ok(())
    }

    /// Steps the worker until its work lane is empty.
    fn until_idle_work(&mut self) {
        while !self.state.work.is_empty() {
            self.state.step(&self.progress);
        }
    }
}
