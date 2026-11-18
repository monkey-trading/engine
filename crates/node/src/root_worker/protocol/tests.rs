//! The handshakes under loom, which runs each model in every interleaving of its threads' atomic
//! operations and fails any in which a thread waits for good.

use std::collections::VecDeque;

use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use loom::sync::{Arc, Mutex, Notify};
use loom::thread;

use super::*;

impl Word for AtomicU64 {
    fn new(value: u64) -> Self {
        AtomicU64::new(value)
    }

    fn load(&self) -> u64 {
        AtomicU64::load(self, Ordering::Acquire)
    }

    fn store(&self, value: u64) {
        AtomicU64::store(self, value, Ordering::Release);
    }

    fn fetch_add(&self, value: u64) -> u64 {
        AtomicU64::fetch_add(self, value, Ordering::AcqRel)
    }

    fn fetch_or(&self, value: u64) -> u64 {
        AtomicU64::fetch_or(self, value, Ordering::AcqRel)
    }

    fn fetch_and(&self, value: u64) -> u64 {
        AtomicU64::fetch_and(self, value, Ordering::AcqRel)
    }
}

#[test]
fn a_deferred_checkpoint_is_woken_once_the_worker_reaches_what_it_waits_for() {
    loom::model(|| {
        let progress = Arc::new(Progress::<AtomicU64>::default());
        let wake = Arc::new(Notify::new());
        let worker = {
            let (progress, wake) = (progress.clone(), wake.clone());
            thread::spawn(move || {
                for _ in 0..2 {
                    if progress.finish(1) {
                        wake.notify();
                    }
                }
            })
        };
        // The sequencer's level check: armed, then waiting only if progress has not already reached
        // the target, whenever the worker's steps fall.
        while progress.finished() < 2 {
            if !progress.wake_when(2) {
                wake.wait();
            }
        }
        worker.join().unwrap();
    });
}

#[test]
fn a_target_already_reached_when_armed_needs_no_wake_and_leaves_none_armed() {
    loom::model(|| {
        let progress = Arc::new(Progress::<AtomicU64>::default());
        let worker = {
            let progress = progress.clone();
            thread::spawn(move || progress.finish(3))
        };
        let woken = worker.join().unwrap();
        assert!(!woken, "nothing was armed while the worker finished");
        assert!(progress.wake_when(3), "progress the worker made before the arming is seen");
        assert!(!progress.finish(1), "the reached target was disarmed");
    });
}

#[test]
fn a_waited_commit_goes_ahead_of_a_read_and_the_read_runs_with_nothing_more_sent() {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(3);
    builder.check(|| {
        let shared = Arc::new(Shared::default());
        let worker = {
            let shared = shared.clone();
            thread::spawn(move || run_worker(&shared, 3))
        };
        // The sequencer: a leaf write, then a commit it waits on, with a read forwarded meanwhile and
        // nothing sent after the commit's answer.
        shared.send(Message::Work(Step::Advance));
        shared.progress.set_commit_waiting(true);
        let reader = {
            let shared = shared.clone();
            thread::spawn(move || shared.send(Message::Read))
        };
        shared.send(Message::Work(Step::Commit));
        // The commit's answer, as its channel gives it: loom's `Notify` may wake with none.
        while !shared.committed.load(Ordering::Acquire) {
            shared.answered.wait();
        }
        shared.progress.set_commit_waiting(false);
        reader.join().unwrap();
        worker.join().unwrap();
        let done = shared.done.lock().unwrap().clone();
        assert_eq!(done, [Done::Work(Step::Advance), Done::Work(Step::Commit), Done::Read]);
    });
}

// ── Helpers ──

/// A step of the sequencer's work lane in the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Advance,
    Commit,
}

enum Message {
    Work(Step),
    Read,
}

/// What the model's worker did, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Done {
    Work(Step),
    Read,
}

/// The model's channels: the worker's inbox and the commit's answer.
#[derive(Default)]
struct Shared {
    progress: Progress<AtomicU64>,
    inbox: Mutex<VecDeque<Message>>,
    arrived: Notify,
    /// The commit's answer, and its wake-up.
    committed: AtomicBool,
    answered: Notify,
    done: Mutex<Vec<Done>>,
}

impl Shared {
    fn send(&self, message: Message) {
        self.inbox.lock().unwrap().push_back(message);
        self.arrived.notify();
    }
}

/// The worker's loop as the node runs it: each message is taken into its lane, each step's lane is
/// `next_lane`'s, a commit clears the flag before it is answered, and it waits for a message only when
/// `next_lane` gives no step. It stops after the `messages` the model sends.
fn run_worker(shared: &Shared, messages: usize) {
    let (mut work, mut reads) = (VecDeque::new(), 0);
    let mut last = Lane::Background;
    let mut handled = 0;
    while handled < messages {
        for message in shared.inbox.lock().unwrap().drain(..) {
            match message {
                Message::Work(step) => work.push_back(step),
                Message::Read => reads += 1,
            }
        }
        let Some(lane) = shared.progress.next_lane(last, !work.is_empty(), reads > 0, false) else {
            shared.arrived.wait();
            continue;
        };
        last = lane;
        handled += 1;
        match lane {
            Lane::Work => {
                let step = work.pop_front().expect("the work lane holds a step");
                shared.done.lock().unwrap().push(Done::Work(step));
                if step == Step::Commit {
                    shared.progress.set_commit_waiting(false);
                    shared.committed.store(true, Ordering::Release);
                    shared.answered.notify();
                }
            }
            Lane::Read => {
                reads -= 1;
                shared.done.lock().unwrap().push(Done::Read);
            }
            Lane::Background => unreachable!("the model queues no background step"),
        }
    }
}
