use super::*;

use std::fs;

use engine_core::command::{Command, Sequenced};
use engine_core::types::AccountId;

use crate::log::segments::segment_name;
use crate::log::{Checkpoint, CheckpointDomain, CommandEntry};
use crate::tee::EnclaveKey;
use crate::test_support::scratch_dir;

const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };

/// Each sealed command's seal, whatever is wrong with it, gets the verdict its own check gives,
/// taken from the workers: a seal by another key, one that is not hex, one over another link, and
/// commands with no seal among them, across checkpoints, with fewer workers than commands in flight.
#[test]
fn every_verdict_is_the_seals_own_check_taken_ahead() {
    let path = log_file("seals-ahead-verdicts");
    let mut entries = sealed(&EnclaveKey::test_key(), 0..60, 7);
    spoil(&mut entries);
    write_file(&path, &entries);
    let mut ahead = SealsAhead::start(&path, 0, B256::default(), workers(3)).expect("the threads start");
    let (mut failed, mut sealed_commands) = (0, 0u64);
    for (seq, seal, link) in seals_with_links(&entries, B256::default()) {
        let taken = ahead.verify(seq, &seal, link);
        assert_eq!(verdict(&taken), verdict(&seal.verify(link)), "seq {seq}");
        failed += usize::from(taken.is_err());
        sealed_commands += 1;
    }
    assert_eq!(failed, 3, "the spoilt seals fail");
    assert_eq!((ahead.ahead, ahead.in_place), (sealed_commands, 0), "every verdict came from the workers");
}

/// A seal the replay holds otherwise than the reader read it, or over another link, is checked in
/// place, so a log that changed between the two reads is still checked on what the replay applies.
#[test]
fn a_seal_or_link_the_reader_did_not_find_is_checked_in_place() {
    let path = log_file("seals-ahead-mismatch");
    let entries = sealed(&EnclaveKey::test_key(), 0..4, 100);
    write_file(&path, &entries);
    let mut ahead = SealsAhead::start(&path, 0, B256::default(), workers(2)).expect("the threads start");
    let mut seals = seals_with_links(&entries, B256::default()).into_iter();
    let (seq, seal, _) = seals.next().unwrap();
    let elsewhere = B256::from([9; 32]);
    assert_eq!(verdict(&ahead.verify(seq, &seal, elsewhere)), verdict(&seal.verify(elsewhere)));
    let (seq, _, link) = seals.next().unwrap();
    let foreign = EnclaveKey::generate().seal_command(DOMAIN, link);
    assert_eq!(verdict(&ahead.verify(seq, &foreign, link)), verdict(&foreign.verify(link)));
    let (seq, seal, link) = seals.next().unwrap();
    ahead.verify(seq, &seal, link).unwrap();
    assert_eq!((ahead.ahead, ahead.in_place), (1, 2));
}

/// A reader started at a command past a segment's first, after a checkpoint, chains from the link it
/// is given and checks ahead from that command on, reading a segment directory in order.
#[test]
fn a_reader_starts_at_its_command_in_a_segment_directory() {
    let dir = scratch_dir("seals-ahead-segments");
    let entries = sealed(&EnclaveKey::test_key(), 0..30, 5);
    write_segments(&dir, &entries, 8);
    let from = 10;
    let checkpoint = entries
        .iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) if checkpoint.seq < from => Some(checkpoint),
            _ => None,
        })
        .unwrap();
    assert_eq!(checkpoint.seq + 1, from, "the reader starts after a checkpoint");
    let link = checkpoint_link(checkpoint).unwrap();
    let mut ahead = SealsAhead::start(&dir, from, link, workers(2)).expect("the threads start");
    let after: Vec<_> = entries.iter().skip_while(|entry| !is_command_at(entry, from)).cloned().collect();
    let checked = seals_with_links(&after, link);
    for (seq, seal, link) in &checked {
        ahead.verify(*seq, seal, *link).unwrap_or_else(|e| panic!("seq {seq}: {e:#}"));
    }
    assert_eq!((ahead.ahead, ahead.in_place), (checked.len() as u64, 0));
}

/// A line the reader cannot read ends its part: the commands from it on are checked in place, with
/// nothing waiting on the reader.
#[test]
fn what_the_reader_cannot_read_is_checked_in_place() {
    let path = log_file("seals-ahead-unreadable");
    let entries = sealed(&EnclaveKey::test_key(), 0..12, 100);
    write_file(&path, &entries);
    let mut text = fs::read_to_string(&path).unwrap();
    let sixth = text.match_indices('\n').nth(5).unwrap().0 + 1;
    text.insert_str(sixth, "not an entry\n");
    fs::write(&path, text).unwrap();
    let mut ahead = SealsAhead::start(&path, 0, B256::default(), workers(2)).expect("the threads start");
    for (seq, seal, link) in seals_with_links(&entries, B256::default()) {
        ahead.verify(seq, &seal, link).unwrap();
    }
    assert_eq!((ahead.ahead, ahead.in_place), (6, 6));
}

/// A log the reader cannot open a second time without taking bytes from the replay's own read starts
/// no checks, and the replay checks every seal in place: a pipe, a regular file reached through a path
/// under `/dev` (macOS's alias of an open descriptor, or Linux's `/dev/shm`), a device, or a log that
/// is not there. A regular file and a directory are read twice.
#[test]
fn a_log_that_cannot_be_read_twice_starts_no_checks() {
    let file = log_file("seals-ahead-twice");
    write_file(&file, &sealed(&EnclaveKey::test_key(), 0..2, 100));
    let dir = file.parent().unwrap();
    assert!(readable_twice(&file) && readable_twice(dir), "a regular file and a directory are read twice");
    let pipe = dir.join("pipe");
    assert!(std::process::Command::new("mkfifo").arg(&pipe).status().unwrap().success());
    let opened = File::open(&file).unwrap();
    let under_dev = UnderDev::of(&opened);
    let missing = dir.join("missing");
    let refused =
        [Some(pipe.as_path()), under_dev.as_ref().map(|found| found.0.as_path()), Some(Path::new("/dev/null"))];
    for path in refused.into_iter().flatten().chain([missing.as_path()]) {
        assert!(SealsAhead::start(path, 0, B256::default(), workers(1)).is_none(), "{}", path.display());
    }
}

/// A directory is read twice wherever it is, as with `TMPDIR=/dev/shm`, since the reader opens each segment
/// afresh and requires it to be a regular file; a refusal says which log and why.
#[test]
fn a_directory_under_dev_is_read_twice_and_a_refusal_is_logged() {
    assert!(readable_twice(Path::new("/dev")), "a directory under /dev");
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let missing = scratch_dir("seals-ahead-refused").join("missing");
    assert!(SealsAhead::start(&missing, 0, B256::default(), workers(1)).is_none());
    assert_eq!(logs.count(&missing.display().to_string()), 1, "the refusal names the path");
}

/// A segment that is not a regular file, such as a pipe no writer opens, ends the reader's read before
/// it opens it, rather than leaving the reader waiting on it while the replay waits to join it.
#[test]
fn a_segment_that_is_a_pipe_ends_the_read() {
    let dir = scratch_dir("seals-ahead-pipe-segment");
    write_segments(&dir, &sealed(&EnclaveKey::test_key(), 0..20, 100), 8);
    let pipe = dir.join(segment_name(8));
    fs::remove_file(&pipe).unwrap();
    assert!(std::process::Command::new("mkfifo").arg(&pipe).status().unwrap().success());
    let mut read = 0;
    let ended = each_entry(&dir, 0, &AtomicBool::new(false), |_| {
        read += 1;
        Ok(())
    });
    assert!(format!("{:#}", ended.unwrap_err()).contains("is not a regular file"));
    assert_eq!(read, 8, "the read ends at the pipe, after the segment before it");
}

/// A command the log holds twice, the second with the same seq, takes the verdict on its own seal, in
/// the order the reader found them, with more commands after them than the reader may run ahead: the
/// repeat's seal is over its first copy's link, so it and every later one fail, as in place.
#[test]
fn a_repeated_seq_takes_the_verdict_on_its_own_seal() {
    let path = log_file("seals-ahead-repeated");
    let count = u64::try_from(AHEAD).unwrap() + 40;
    let mut entries = sealed(&EnclaveKey::test_key(), 0..count, 2000);
    let repeated = entries[5].clone();
    entries.insert(6, repeated);
    write_file(&path, &entries);
    let mut ahead = SealsAhead::start(&path, 0, B256::default(), workers(3)).expect("the threads start");
    let checked = seals_with_links(&entries, B256::default());
    let failed: Vec<_> = checked
        .iter()
        .enumerate()
        .filter(|(_, (seq, seal, link))| {
            let taken = ahead.verify(*seq, seal, *link);
            assert_eq!(verdict(&taken), verdict(&seal.verify(*link)), "seq {seq}");
            taken.is_err()
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(failed.first(), Some(&6), "the repeat is the first seal to fail");
    assert_eq!((ahead.ahead, ahead.in_place), (checked.len() as u64, 0), "every verdict came from the workers");
}

/// A replay whose commands part from the reader's, as when the host serves the two reads different
/// bytes, stops the checks ahead at the first sealed command the reader passed and the replay never
/// reached, and checks that seal and every later one in place.
#[test]
fn reads_that_part_stop_the_checks_ahead() {
    let path = log_file("seals-ahead-parted");
    let entries = sealed(&EnclaveKey::test_key(), 0..20, 100);
    write_file(&path, &entries);
    let mut ahead = SealsAhead::start(&path, 0, B256::default(), workers(2)).expect("the threads start");
    let seals = seals_with_links(&entries, B256::default());
    let (seq, seal, link) = &seals[0];
    ahead.verify(*seq, seal, *link).unwrap();
    for (seq, seal, link) in &seals[2..] {
        assert_eq!(verdict(&ahead.verify(*seq, seal, *link)), verdict(&seal.verify(*link)), "seq {seq}");
    }
    assert!(ahead.live.is_none(), "the reader and the workers are stopped");
    assert_eq!((ahead.ahead, ahead.in_place), (1, 18));
}

/// A replay that stops early drops the checks while the reader and the workers are mid-log, in a log
/// longer than the reader may run ahead: the drop closes their channels and joins them all, so the reader
/// has ended when it returns.
#[test]
fn dropping_mid_log_stops_and_joins_the_threads() {
    let path = log_file("seals-ahead-bound");
    let count = u64::try_from(AHEAD).unwrap() * 2;
    let entries = sealed(&EnclaveKey::test_key(), 0..count, 1000);
    write_file(&path, &entries);
    let mut ahead = SealsAhead::start(&path, 0, B256::default(), workers(2)).expect("the threads start");
    let (seq, seal, link) = seals_with_links(&entries, B256::default()).remove(0);
    ahead.verify(seq, &seal, link).unwrap();
    assert_eq!(ahead.ahead, 1);
    // The reader's closure holds the stop flag until its thread ends, so once the drop has joined it
    // nothing holds the flag; a drop that left it running would still be wound down, past the drop's return.
    let flag = Arc::downgrade(&ahead.live.as_ref().expect("the checks are live").stop);
    assert_eq!(flag.strong_count(), 2, "the replay's end and the reader's");
    drop(ahead);
    assert_eq!(flag.strong_count(), 0, "the reader has ended by the time the drop returns");
}

// ── Helpers ──

/// A regular file reached through a path under `/dev`: macOS's alias of an open descriptor, or a file in
/// Linux's `/dev/shm`, removed when dropped. None on a system with neither.
struct UnderDev(PathBuf, bool);

impl Drop for UnderDev {
    /// Removes the file it made, if it made one.
    fn drop(&mut self) {
        if self.1 {
            let _ = fs::remove_file(&self.0);
        }
    }
}

impl UnderDev {
    /// `opened`'s alias under `/dev/fd`.
    #[cfg(target_os = "macos")]
    fn of(opened: &File) -> Option<Self> {
        use std::os::fd::AsRawFd;
        Some(Self(PathBuf::from(format!("/dev/fd/{}", opened.as_raw_fd())), false))
    }

    /// A new file in `/dev/shm`, if the system has one.
    #[cfg(not(target_os = "macos"))]
    fn of(_: &File) -> Option<Self> {
        let path = PathBuf::from(format!("/dev/shm/engine-node-test-{}-seals-ahead", std::process::id()));
        fs::write(&path, "").ok().map(|()| Self(path, true))
    }
}

/// A log file in a new scratch directory named `name`.
fn log_file(name: &str) -> PathBuf {
    let dir = scratch_dir(name);
    fs::create_dir_all(&dir).unwrap();
    dir.join("log.jsonl")
}

/// `count` workers.
fn workers(count: usize) -> ReplayWorkers {
    ReplayWorkers(NonZeroUsize::new(count).unwrap())
}

/// A log of ticks at `seqs`, each sealed by `key` over its link as a writer chains them, with a
/// checkpoint after every `every`th, each restarting the chain.
fn sealed(key: &EnclaveKey, seqs: std::ops::Range<u64>, every: u64) -> Vec<LogEntry> {
    let mut link = B256::default();
    let mut entries = Vec::new();
    for seq in seqs {
        let mut command = CommandEntry {
            sequenced: Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick },
            events: Some(Vec::new()),
            seal: None,
        };
        link = command_link(link, &command).unwrap();
        command.seal = Some(key.seal_command(DOMAIN, link));
        entries.push(LogEntry::Command(command));
        if (seq + 1) % every == 0 {
            let checkpoint = Checkpoint {
                parent: None,
                seq,
                root: format!("0x{}", hex::encode([u8::try_from(seq % 256).unwrap(); 32])),
                signature: String::new(),
                signer: key.address_hex(),
                domain: Some(DOMAIN),
                deferred_withdrawals: false,
            };
            link = checkpoint_link(&checkpoint).unwrap();
            entries.push(LogEntry::Checkpoint(checkpoint));
        }
    }
    entries
}

/// Spoils three seals and drops one: a seal by another key, a signature that is not hex, a seal over
/// another link, and a command with none.
fn spoil(entries: &mut [LogEntry]) {
    let other = EnclaveKey::generate();
    let mut commands = entries.iter_mut().filter_map(|entry| match entry {
        LogEntry::Command(command) => Some(command),
        _ => None,
    });
    let foreign = commands.nth(3).unwrap();
    foreign.seal = Some(other.seal_command(DOMAIN, command_link(B256::default(), foreign).unwrap()));
    commands.nth(10).unwrap().seal.as_mut().unwrap().signature = "0xnot-hex".into();
    let elsewhere = commands.nth(10).unwrap();
    elsewhere.seal = Some(EnclaveKey::test_key().seal_command(DOMAIN, B256::from([3; 32])));
    commands.nth(10).unwrap().seal = None;
}

/// Each sealed command's seq, seal and link, chained from `link` as the replay chains them.
fn seals_with_links(entries: &[LogEntry], mut link: B256) -> Vec<(u64, CommandSeal, B256)> {
    let mut seals = Vec::new();
    for entry in entries {
        match entry {
            LogEntry::Command(command) => {
                link = command_link(link, command).unwrap();
                if let Some(seal) = &command.seal {
                    seals.push((command.sequenced.seq, seal.clone(), link));
                }
            }
            LogEntry::Checkpoint(checkpoint) => link = checkpoint_link(checkpoint).unwrap(),
            LogEntry::Withdrawal(_) => {}
        }
    }
    seals
}

/// Whether `entry` is the command at `seq`.
fn is_command_at(entry: &LogEntry, seq: u64) -> bool {
    matches!(entry, LogEntry::Command(command) if command.sequenced.seq == seq)
}

/// A verdict as it reads: passed, or the whole error.
fn verdict(result: &anyhow::Result<()>) -> Result<(), String> {
    result.as_ref().map(|_| ()).map_err(|e| format!("{e:#}"))
}

/// `entries` as one log file at `path`.
fn write_file(path: &Path, entries: &[LogEntry]) {
    fs::write(path, lines(entries)).unwrap();
}

/// `entries` as segment files in `dir`, a new one at every `every`th command.
fn write_segments(dir: &Path, entries: &[LogEntry], every: u64) {
    fs::create_dir_all(dir).unwrap();
    let mut segments: BTreeMap<u64, Vec<LogEntry>> = BTreeMap::new();
    let mut first = 0;
    for entry in entries {
        if let LogEntry::Command(command) = entry
            && command.sequenced.seq % every == 0
        {
            first = command.sequenced.seq;
        }
        segments.entry(first).or_default().push(entry.clone());
    }
    for (first, entries) in segments {
        fs::write(dir.join(segment_name(first)), lines(&entries)).unwrap();
    }
}

/// `entries`, one JSON line each.
fn lines(entries: &[LogEntry]) -> String {
    entries.iter().map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap())).collect()
}
