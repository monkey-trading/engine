//! One switch at a turn's boundary under load: the turn's member, following the incumbent, takes
//! only the tail past its tip; the incumbent stops; the member's node starts from its replica; then
//! the incumbent follows its successor. The window runs on the wall clock from the incumbent's last
//! published command to the member's `TurnTaken`, and the member replays no whole log inside it.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use engine_core::command::Command;
use engine_core::types::AccountId;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use super::super::chain::Chain;
use super::load;
use super::seat::{Holder, Member};
use crate::api::ApiState;
use crate::checkpoints::{CheckpointChain, ScheduledTurn, View};
use crate::clock::test_clock::TestClock;
use crate::config::NodeConfig;
use crate::log::{LogEntry, Manifest, Offset, offset_of};
use crate::metrics::Metrics;
use crate::node::ready::Start;
use crate::tee::sealing::Release;
use crate::test_support::wait_for;

/// The members' waits for their turns and the load, as the scenario drives them.
pub(super) struct Members {
    taken: mpsc::Receiver<(usize, Box<Start>)>,
    resume: Vec<Arc<Notify>>,
    pub(super) metrics: Vec<Arc<Metrics>>,
    load: watch::Sender<Option<ApiState>>,
    /// What the load got done.
    pub(super) done: Arc<load::Load>,
    /// Each member's wait, by index, then the load's.
    tasks: Vec<JoinHandle<()>>,
    /// What starts a member's wait again.
    again: Again,
}

/// What starts a member's wait again: the members with their configs, the clock, and where a wait hands
/// its start.
type Again = (Vec<(Member, &'static NodeConfig)>, Arc<TestClock>, mpsc::Sender<(usize, Box<Start>)>);

impl Members {
    /// Starts each member's wait for its turns on `clock`: all but `holder`'s from boot, `holder`'s
    /// once it first hands its turn over. The load runs on `holder`'s node.
    pub(super) fn start(chain: &Chain, members: &[Member], holder: &Holder, clock: &Arc<TestClock>) -> Self {
        let (taken_tx, taken) = mpsc::channel(1);
        let (mut resume, mut metrics, mut tasks, mut again) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (index, member) in members.iter().enumerate() {
            let (notify, counted) = (Arc::new(Notify::new()), Arc::new(Metrics::new()));
            let cfg: &'static NodeConfig = Box::leak(Box::new(member.config(chain)));
            again.push((member.clone(), cfg));
            let (member, clock, shared) = (member.clone(), clock.clone(), (taken_tx.clone(), notify.clone()));
            let from_boot = index != holder.member;
            let follows = counted.clone();
            tasks.push(tokio::task::spawn_local(async move {
                member.follow(cfg, index, clock, follows, shared, from_boot).await;
            }));
            resume.push(notify);
            metrics.push(counted);
        }
        let (load, holding) = watch::channel(Some(holder.node.state.clone()));
        let (done, generating) = load::generate(holding, AccountId(chain.vault.into()), clock.clone());
        tasks.push(generating);
        let again = (again, clock.clone(), taken_tx);
        Self { taken, resume, metrics, load, done, tasks, again }
    }

    /// Stops `member`'s wait while it follows: its replica and its log check answers go with it.
    pub(super) fn pause(&self, member: usize) {
        self.tasks[member].abort();
    }

    /// Starts `member`'s wait again from boot, as a restarted member's node does: it opens its copy
    /// and follows the operator in effect.
    pub(super) fn restart(&mut self, member: usize) {
        let ((waiting, cfg), clock) = (self.again.0[member].clone(), self.again.1.clone());
        let shared = (self.again.2.clone(), self.resume[member].clone());
        let counted = self.metrics[member].clone();
        self.tasks[member] = tokio::task::spawn_local(async move {
            waiting.follow(cfg, member, clock, counted, shared, true).await;
        });
    }

    /// Puts the load on `holder`'s node.
    pub(super) fn load(&self, holder: &Holder) {
        self.load.send_replace(Some(holder.node.state.clone()));
    }

    /// Waits until the load has placed and cancelled more orders than it had: it succeeds on the
    /// node that holds the turn.
    pub(super) async fn loaded(&self) {
        let done = self.done.clone();
        let (placed, cancelled) = (done.placed.load(Ordering::Relaxed), done.cancelled.load(Ordering::Relaxed));
        let progressed = async {
            while done.placed.load(Ordering::Relaxed) <= placed || done.cancelled.load(Ordering::Relaxed) <= cancelled {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        if tokio::time::timeout(crate::test_support::LOADED_WAIT, progressed).await.is_err() {
            panic!("the load placed and cancelled no order; the last refused: {:?}", done.refused.lock().unwrap());
        }
    }

    /// Waits until `member` has built its replica `opened` times: it follows the operator in effect.
    pub(super) async fn following(&self, member: usize, opened: u64) {
        let metrics = self.metrics[member].clone();
        wait_for("a member to build its replica and follow", async move {
            while metrics.full_replays.get() < opened {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
    }

    /// Stops the members' waits and the load.
    pub(super) fn stop(self) {
        self.load.send_replace(None);
        self.tasks.iter().for_each(JoinHandle::abort);
    }
}

/// What a switch left.
pub(super) struct Switched {
    pub(super) holder: Holder,
    /// The incumbent's final checkpoint's seq.
    pub(super) head: u64,
    /// The turn the finalised schedule named, at the incumbent's reach of the boundary.
    pub(super) scheduled: ScheduledTurn,
    /// The release and generation the member sealed the keys under.
    pub(super) sealed: (Release, u64),
    /// From the incumbent's last published command to the member's `TurnTaken`, on the wall clock.
    pub(super) window: Duration,
    /// The bytes and entries the tail carried, and the bytes of the whole log.
    pub(super) moved: (u64, u64, u64),
}

/// Switches from `holder` to `members[member]` under load: the incumbent releases the turn to the
/// member's wait, which hands over its start; the member's node starts from the replica and
/// sequences `TurnTaken`; then the incumbent follows its successor. Asserts that the member replayed
/// no whole log inside the window and that only the incumbent's entries past the member's tip moved.
pub(super) async fn switch(
    chain: &Chain,
    members: &[Member],
    holder: Holder,
    member: usize,
    clock: Arc<TestClock>,
    shared: &mut Members,
) -> Switched {
    let cfg = members[member].config(chain);
    let vault = crate::handover::vault_reader(&cfg).unwrap().expect("[checkpoints] reads the vault");
    let reached = holder.node.trusted().reached_ms().expect("the node holds a trusted time sample");
    let at = vault.observe(View::Finalized, &[]).await.unwrap();
    let scheduled = vault.scheduled_turn(&at, reached).await.unwrap().expect("the finalised schedule names a turn");
    let (incumbent, replays) = (holder.member, shared.metrics[member].full_replays.get());
    // The load succeeds on the incumbent up to the switch.
    shared.loaded().await;
    let (released, taken) = tokio::join!(holder.release(), shared.taken.recv());
    let (index, start) = taken.expect("the member's wait hands over its start");
    assert_eq!(index, member, "another member took the turn");
    let sealed = (start.store.release(), start.store.generation());
    // A minute of the turn passes with no node sequencing, then the member's node starts from the
    // replica and sequences its first command.
    clock.advance(Duration::from_secs(60));
    let source = released.source;
    // The member's own metrics count a replay its node's start would run, inside the window.
    let measured = (member, shared.metrics[member].clone());
    let (holder, moved) = Holder::from_replica(chain, members, measured, (start, source), clock).await;
    holder.node.submit(Command::Tick).await;
    let stamps = holder.node.stamps.clone();
    let turn_taken = wait_for("the member's TurnTaken", async move {
        loop {
            if let Some(at) = stamps.lock().unwrap().turn_taken {
                break at;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let window = turn_taken.saturating_duration_since(released.last_command);
    assert_eq!(shared.metrics[member].full_replays.get(), replays, "the member replayed a whole log in the window");
    let (bytes, entries, whole) = tail_of(&released.dir, moved.after);
    assert_eq!(moved.bytes, bytes, "the handover moved more than the tail");
    assert!(moved.entries <= entries, "the copy took {} entries from a tail of {entries}", moved.entries);
    // The load moves to the member's node, and succeeds there; the incumbent follows its successor.
    shared.load(&holder);
    shared.loaded().await;
    let opened = shared.metrics[incumbent].full_replays.get() + 1;
    shared.resume[incumbent].notify_one();
    shared.following(incumbent, opened).await;
    Switched { holder, head: released.head, scheduled, sealed, window, moved: (bytes, entries, whole) }
}

/// The bytes and the entries, as the log's own lines, that the log in `dir` holds after the command at
/// `after`, and the bytes of the whole log.
fn tail_of(dir: &Path, after: u64) -> (u64, u64, u64) {
    let manifest = Manifest::load(dir).unwrap();
    let (mut bytes, mut entries, mut whole) = (0, 0, 0);
    for listed in manifest.segments.iter() {
        let segment = std::fs::read(dir.join(&listed.file)).unwrap();
        whole += segment.len() as u64;
        let mut previous = None;
        for line in segment.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
            let entry: LogEntry = serde_json::from_slice(line).unwrap();
            let offset = offset_of(previous, &entry).unwrap();
            previous = Some(offset);
            if offset > Offset::start_of(after) {
                bytes += line.len() as u64 + 1;
                entries += 1;
            }
        }
    }
    (bytes, entries, whole)
}
