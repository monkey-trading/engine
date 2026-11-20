//! Command seals checked ahead of a replay. A reader of the log's files of its own finds each sealed
//! command's link as the replay will compute it, and a pool of workers checks the seal over that link
//! while the replay applies the commands before it. The replay still computes every link itself, and
//! takes a worker's verdict only for the seal and link it computed, checking any other seal in place:
//! the verdict it acts on is always `CommandSeal::verify` of its own inputs, for the command it applies
//! next, so a seal that fails fails at the same command with the same error as a replay that checks
//! every seal in place. The reader, the workers and the replay share only channels and a stop flag.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::BufReader;
use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use engine_core::eip712::B256;
use serde::Deserialize;
use tracing::{info, warn};

use crate::log::command_seal::{checkpoint_link, command_link};
use crate::log::segments::list_segments;
use crate::log::{CommandSeal, LogEntry, Stop, parse_line, read_lines};

/// How many commands the reader may pass that the replay has not reached.
const AHEAD: usize = 1024;

/// How many threads check command seals ahead of a replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct ReplayWorkers(NonZeroUsize);

/// The reader and workers checking a replay's command seals ahead of it; stopped and joined when
/// dropped.
pub(crate) struct SealsAhead {
    /// The reader and the workers, until the replay is done with them or its commands and the
    /// reader's part.
    live: Option<Live>,
    workers: usize,
    /// Seals the replay took a worker's verdict for, and seals it checked in place.
    ahead: u64,
    in_place: u64,
}

/// The replay's ends of the reader's and the workers' channels, and their threads.
struct Live {
    /// Each command the reader passes, in log order.
    found: Receiver<Found>,
    /// The workers' verdicts, as they finish them: one for each job, and a job only for a command the
    /// reader passed and the replay has not reached, so `found`'s bound and the job queue's bound them.
    verdicts: Receiver<Verdict>,
    /// A command the reader passed that the replay has not reached.
    next: Option<Found>,
    /// Verdicts that came before the replay reached their commands, by job.
    arrived: BTreeMap<u64, Verdict>,
    /// Tells the reader to stop at its next line.
    stop: Arc<AtomicBool>,
    /// The threads, joined after the channels above close, which ends them.
    threads: Joined,
}

/// A command the reader passed, and the job that checks its seal, if one does.
struct Found {
    seq: u64,
    job: Option<u64>,
}

/// A seal for a worker to check over `link`, numbered in the order the reader found it.
struct Job {
    id: u64,
    seal: CommandSeal,
    link: B256,
}

/// A worker's verdict on job `job`'s seal over its link: `None` when the check panicked.
struct Verdict {
    job: u64,
    seal: CommandSeal,
    link: B256,
    result: Option<anyhow::Result<()>>,
}

/// What the workers have on the seal of the command the replay applies next.
enum Ahead {
    /// Their verdict on the seal and link the replay computed.
    Verdict(anyhow::Result<()>),
    /// None on that seal and link: the replay checks the seal in place.
    InPlace,
    /// The reader passed a sealed command the replay never reached: the two reads of the log have
    /// parted, and the replay checks this seal and every later one in place.
    Parted,
}

/// Threads joined when dropped.
struct Joined(Vec<JoinHandle<()>>);

impl Drop for Joined {
    /// Joins the threads.
    fn drop(&mut self) {
        for thread in self.0.drain(..) {
            // A thread that panicked dropped its channels' ends, and the replay checks in place what it left.
            let _ = thread.join();
        }
    }
}

impl Drop for Live {
    /// Tells the reader to stop; dropping the fields then closes the channels and joins the threads.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for SealsAhead {
    /// Logs how the replay's seals were checked, if it checked any.
    fn drop(&mut self) {
        if self.ahead + self.in_place > 0 {
            info!(
                workers = self.workers,
                ahead = self.ahead,
                in_place = self.in_place,
                "checked command seals ahead of the replay"
            );
        }
    }
}

impl Default for ReplayWorkers {
    /// One worker per core (`available`).
    fn default() -> Self {
        Self::available()
    }
}

impl From<NonZeroUsize> for ReplayWorkers {
    /// `count` workers.
    fn from(count: NonZeroUsize) -> Self {
        Self(count)
    }
}

impl ReplayWorkers {
    /// One worker per core this process may use, or one when that is unknown.
    pub fn available() -> Self {
        Self(thread::available_parallelism().unwrap_or(NonZeroUsize::MIN))
    }
}

impl SealsAhead {
    /// Starts reading the log at `path`, a segment directory or a single file, from its first command
    /// at or after `from`, whose link follows `link`, and `workers` checking the seals it finds. `None`
    /// when the log cannot be read a second time (`readable_twice`) or a thread cannot start: the replay
    /// then checks every seal in place.
    pub(crate) fn start(path: &Path, from: u64, link: B256, workers: ReplayWorkers) -> Option<Self> {
        if !readable_twice(path) {
            return None;
        }
        let workers = workers.0.get();
        let (found_tx, found) = sync_channel(AHEAD);
        let (jobs_tx, jobs) = sync_channel(2 * workers);
        let (verdicts_tx, verdicts) = channel();
        // The workers take their jobs from one queue.
        let jobs = Arc::new(Mutex::new(jobs));
        let stop = Arc::new(AtomicBool::new(false));
        let mut live = Live {
            found,
            verdicts,
            next: None,
            arrived: BTreeMap::new(),
            stop: stop.clone(),
            threads: Joined(Vec::new()),
        };
        let path = path.to_owned();
        let reader = thread::Builder::new()
            .name("seal-reader".into())
            .spawn(move || read(&path, (from, link), &stop, found_tx, jobs_tx));
        let spawned = reader.map(|reader| live.threads.0.push(reader)).and_then(|()| {
            (0..workers).try_for_each(|n| {
                let (jobs, verdicts) = (jobs.clone(), verdicts_tx.clone());
                let worker =
                    thread::Builder::new().name(format!("seal-check-{n}")).spawn(move || check(&jobs, &verdicts));
                worker.map(|worker| live.threads.0.push(worker))
            })
        });
        // Only the threads hold the queue and the verdicts' sender, so their ends close when the threads end.
        drop((jobs, verdicts_tx));
        match spawned {
            Ok(()) => Some(Self { live: Some(live), workers, ahead: 0, in_place: 0 }),
            Err(e) => {
                warn!("the replay checks each command seal in place: a seal check thread did not start: {e}");
                None
            }
        }
    }

    /// `seal.verify(link)` for the command at `seq`: the workers' verdict when they checked this seal
    /// over this link, waited for while they may still give it, and otherwise checked in place.
    /// Error cases:
    /// - as `CommandSeal::verify`
    pub(crate) fn verify(&mut self, seq: u64, seal: &CommandSeal, link: B256) -> anyhow::Result<()> {
        let ahead = self.live.as_mut().map_or(Ahead::InPlace, |live| live.take(seq, seal, link));
        match ahead {
            Ahead::Verdict(verdict) => {
                self.ahead += 1;
                return verdict;
            }
            Ahead::Parted => {
                warn!(
                    seq,
                    "the seal reader's commands part from the replay's here: each later seal is checked in place"
                );
                self.live = None;
            }
            Ahead::InPlace => {}
        }
        self.in_place += 1;
        seal.verify(link)
    }

    /// How many seals' verdicts the replay took from the workers.
    #[cfg(test)]
    pub(crate) fn taken_ahead(&self) -> u64 {
        self.ahead
    }
}

impl Live {
    /// What the workers have on `seal` over `link` for the command at `seq`, waiting for their verdict
    /// while they may still give one.
    fn take(&mut self, seq: u64, seal: &CommandSeal, link: B256) -> Ahead {
        let found = loop {
            let found = match self.next.take() {
                Some(found) => found,
                None => match self.found.recv() {
                    Ok(found) => found,
                    Err(_) => return Ahead::InPlace,
                },
            };
            if found.seq >= seq {
                break found;
            }
            // The replay asks about every sealed command it reaches: one it passed was never in its log.
            if found.job.is_some() {
                return Ahead::Parted;
            }
        };
        if found.seq > seq {
            self.next = Some(found);
            return Ahead::InPlace;
        }
        let Some(job) = found.job else { return Ahead::InPlace };
        let verdict = loop {
            if let Some(verdict) = self.arrived.remove(&job) {
                break verdict;
            }
            match self.verdicts.recv() {
                Ok(verdict) => {
                    self.arrived.insert(verdict.job, verdict);
                }
                Err(_) => return Ahead::InPlace,
            }
        };
        match verdict.result {
            Some(result) if verdict.seal == *seal && verdict.link == link => Ahead::Verdict(result),
            _ => Ahead::InPlace,
        }
    }
}

/// Whether the log at `path` can be read a second time without taking bytes from the replay's own
/// read: a directory or a regular file, and not a pipe, a device or a descriptor's alias under `/dev`
/// or `/proc`, which a second open may share with the first.
fn readable_twice(path: &Path) -> bool {
    fs::canonicalize(path).is_ok_and(|real| {
        !real.starts_with("/dev")
            && !real.starts_with("/proc")
            && fs::metadata(&real).is_ok_and(|found| found.is_dir() || found.is_file())
    })
}

/// The reader: from the first command at or after `from`, chains each command's link from `link` as
/// the replay does, restarting at each checkpoint, sends each sealed command's seal and link to the
/// workers as a job and each command to the replay. It stops at the log's end, at an entry it cannot
/// read, from which the replay checks in place, or once the replay is done (`stop`).
fn read(path: &Path, (from, link): (u64, B256), stop: &AtomicBool, found: SyncSender<Found>, jobs: SyncSender<Job>) {
    let (mut link, mut started, mut jobs_sent) = (Some(link), false, 0);
    // The read ends at the log's end or at what stops it; either way the replay checks the rest in place.
    let _ = each_entry(path, from, stop, |entry| {
        match entry {
            LogEntry::Command(command) => {
                let seq = command.sequenced.seq;
                if !started && seq < from {
                    return Ok(());
                }
                started = true;
                link = link.and_then(|link| command_link(link, &command).ok());
                let job = match (command.seal, link) {
                    (Some(seal), Some(link)) => {
                        jobs.send(Job { id: jobs_sent, seal, link }).map_err(|_| Stop)?;
                        jobs_sent += 1;
                        Some(jobs_sent - 1)
                    }
                    _ => None,
                };
                found.send(Found { seq, job }).map_err(|_| Stop)?;
            }
            LogEntry::Checkpoint(checkpoint) if started => link = checkpoint_link(&checkpoint).ok(),
            LogEntry::Checkpoint(_) | LogEntry::Withdrawal(_) => {}
        }
        Ok(())
    });
}

/// A worker: checks each seal it takes and sends the verdict, until the reader or the replay is done.
fn check(jobs: &Mutex<Receiver<Job>>, verdicts: &Sender<Verdict>) {
    loop {
        let job = jobs.lock().expect("no worker panics while it takes a job").recv();
        let Ok(Job { id, seal, link }) = job else { return };
        let result = panic::catch_unwind(AssertUnwindSafe(|| seal.verify(link))).ok();
        if verdicts.send(Verdict { job: id, seal, link, result }).is_err() {
            return;
        }
    }
}

/// Hands `visit` each entry of the log at `path` in order, until `stop` is set: from the segment
/// holding `from` in a segment directory, or the whole of a single file. A segment that is not a
/// regular file, such as a pipe, whose open could wait for a writer that never comes, ends the read.
/// Error cases:
/// - a file cannot be listed, opened or read, is not a regular file, or holds a line that is no entry
/// - `visit` fails, or `stop` is set (`Stop`)
fn each_entry(
    path: &Path,
    from: u64,
    stop: &AtomicBool,
    mut visit: impl FnMut(LogEntry) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let files: Vec<PathBuf> = if path.is_dir() {
        let segments = list_segments(path)?;
        let first = segments.iter().rposition(|(first_seq, _)| *first_seq <= from).unwrap_or(0);
        segments.into_iter().skip(first).map(|(_, file)| file).collect()
    } else {
        vec![path.to_owned()]
    };
    for file in files {
        anyhow::ensure!(!stop.load(Ordering::Relaxed), Stop);
        anyhow::ensure!(fs::metadata(&file)?.is_file(), "{} is not a regular file", file.display());
        read_lines(BufReader::new(File::open(file)?), |n, line| {
            anyhow::ensure!(!stop.load(Ordering::Relaxed), Stop);
            match parse_line(n, line)? {
                Some(entry) => visit(entry),
                None => Ok(()),
            }
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
