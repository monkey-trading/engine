//! The handshakes under loom, which runs each model in every interleaving of its threads' atomic
//! operations and fails any in which a thread waits for good.

use std::collections::VecDeque;

use loom::sync::atomic::{AtomicU64, Ordering};
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
fn a_waited_commit_is_answered_ahead_of_reads_and_work_keeps_its_order() {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(3);
    builder.check(|| {
        let shared = Arc::new(Shared::default());
        let worker = {
            let shared = shared.clone();
            thread::spawn(move || run_worker(&shared))
        };
        let reader = {
            let shared = shared.clone();
            thread::spawn(move || shared.send(Message::Read))
        };
        // The sequencer: a leaf write, then a commit it waits on, then the release it answers.
        shared.send(Message::Work(Step::Advance));
        shared.progress.set_commit_waiting(true);
        shared.send(Message::Work(Step::Commit));
        shared.answered.wait();
        shared.progress.set_commit_waiting(false);
        shared.send(Message::Work(Step::Release));
        reader.join().unwrap();
        worker.join().unwrap();
        let done = shared.done.lock().unwrap().clone();
        let work: Vec<Step> =
            done.iter().filter_map(|done| if let Done::Work(step) = done { Some(*step) } else { None }).collect();
        assert_eq!(work, [Step::Advance, Step::Commit, Step::Release]);
        assert!(done.contains(&Done::Read));
    });
}

// ── Helpers ──

/// A step of the sequencer's work lane in the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Advance,
    Commit,
    Release,
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
/// `next_lane`'s, and it waits for a message only with every lane empty. It stops after the four
/// messages the model sends.
fn run_worker(shared: &Shared) {
    let (mut work, mut reads) = (VecDeque::new(), 0);
    let mut last = Lane::Background;
    let mut handled = 0;
    while handled < 4 {
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
