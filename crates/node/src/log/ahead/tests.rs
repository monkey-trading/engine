//! A log read ahead hands its caller the entries, the result and the error an in-place read gives.

use super::*;

use engine_core::command::{Command, Sequenced};

use crate::log::CommandEntry;
use crate::test_support::scratch_dir;

/// Commands a segment holds.
const SEGMENT: u64 = 16;
/// Past the most entries the reader holds ahead of its caller.
const COMMANDS: u64 = 3 * AHEAD as u64;

/// A segmented log and a single file read ahead give the caller every entry in order, and the read
/// what an in-place read gives.
#[test]
fn a_log_read_ahead_gives_the_entries_and_the_end_an_in_place_read_gives() {
    let dir = scratch_dir("ahead-read");
    write_log(&dir, COMMANDS);
    let (mut in_place, mut read) = (Vec::new(), Vec::new());
    let opened = SegmentedLog::open(&dir, |entry| {
        in_place.push(entry);
        Ok(())
    })
    .unwrap();
    let ahead = SegmentedLog::open_ahead(&dir, |entry| {
        read.push(entry);
        Ok(())
    })
    .unwrap();
    assert_eq!(read, in_place);
    assert_eq!(in_place.len(), COMMANDS as usize);
    assert_eq!(
        (ahead.last_seq(), ahead.last_listed_segment(), ahead.has_open_tail()),
        (opened.last_seq(), opened.last_listed_segment(), opened.has_open_tail())
    );
    let file = dir.join("whole.jsonl");
    std::fs::write(
        &file,
        in_place.iter().map(|e| format!("{}\n", serde_json::to_string(e).unwrap())).collect::<String>(),
    )
    .unwrap();
    let mut from_file = Vec::new();
    read_file_ahead(&file, |entry| {
        from_file.push(entry);
        Ok(())
    })
    .unwrap();
    assert_eq!(from_file, in_place);
}

/// An entry the caller refuses ends the read with the caller's error, in the context an in-place read
/// gives it, at the first entry, inside the reader's lead and past it, in a segmented log and in a
/// single file, though the reader read further.
#[test]
fn an_entry_the_caller_refuses_ends_the_read_with_the_error_an_in_place_read_gives() {
    let dir = scratch_dir("ahead-refused");
    write_log(&dir, COMMANDS);
    let file = dir.join("whole.jsonl");
    let mut lines = String::new();
    SegmentedLog::open(&dir, |entry| {
        lines.push_str(&format!("{}\n", serde_json::to_string(&entry).unwrap()));
        Ok(())
    })
    .unwrap();
    std::fs::write(&file, lines).unwrap();
    for refused in [0, 7, SEGMENT, COMMANDS / 2, COMMANDS - 1] {
        let reads: [(&str, Reading); 4] = [
            ("segments in place", |path, visit| SegmentedLog::open(path, visit).map(drop)),
            ("segments ahead", |path, visit| SegmentedLog::open_ahead(path, visit).map(drop)),
            ("file in place", |path, visit| read_file(&path.join("whole.jsonl"), visit)),
            ("file ahead", |path, visit| read_file_ahead(&path.join("whole.jsonl"), visit)),
        ];
        let outcomes: Vec<(String, Vec<u64>)> = reads
            .iter()
            .map(|(_, read)| {
                let mut taken = Vec::new();
                let error = read(&dir, &mut |entry| {
                    let seq = seq_of(&entry);
                    anyhow::ensure!(seq != refused, "refused {seq}");
                    taken.push(seq);
                    Ok(())
                })
                .expect_err("the caller refused an entry");
                (format!("{error:#}"), taken)
            })
            .collect();
        for pair in [(0, 1), (2, 3)] {
            let case = format!("{} against {}, refused at {refused}", reads[pair.1].0, reads[pair.0].0);
            assert_eq!(outcomes[pair.1], outcomes[pair.0], "{case}");
        }
        assert!(outcomes[1].0.starts_with("segment ") && outcomes[3].0.starts_with("log "), "{outcomes:?}");
        assert_eq!(outcomes[1].1, (0..refused).collect::<Vec<_>>(), "refused at {refused}");
    }
}

/// A listed segment that no longer matches its listing ends a segmented read with the error an
/// in-place read gives, once the caller has taken every entry of the segments before it.
#[test]
fn a_segment_the_read_refuses_ends_it_with_the_in_place_error_after_every_earlier_entry() {
    let dir = scratch_dir("ahead-altered-segment");
    write_log(&dir, COMMANDS);
    let segments = list_segments(&dir).unwrap();
    let (first_seq, altered) = &segments[segments.len() / 2];
    let mut bytes = std::fs::read(altered).unwrap();
    let at = bytes.iter().position(|byte| *byte == b'1').expect("a digit to change");
    bytes[at] = b'2';
    std::fs::write(altered, bytes).unwrap();
    let (mut in_place, mut ahead) = (Vec::new(), Vec::new());
    let read = SegmentedLog::open(&dir, |entry| {
        in_place.push(seq_of(&entry));
        Ok(())
    });
    let Err(in_place_error) = read else { panic!("an altered segment") };
    let read = SegmentedLog::open_ahead(&dir, |entry| {
        ahead.push(seq_of(&entry));
        Ok(())
    });
    let Err(ahead_error) = read else { panic!("an altered segment") };
    assert_eq!(format!("{ahead_error:#}"), format!("{in_place_error:#}"));
    assert_eq!(ahead, in_place);
    assert_eq!(ahead, (0..*first_seq).collect::<Vec<_>>(), "every entry before the altered segment");
}

/// A read that panics raises its panic in the caller once the caller has taken every entry it sent
/// before it, as a read in place panics there.
#[test]
fn a_read_that_panics_raises_its_panic_after_every_earlier_entry() {
    let read: Read<()> = Box::new(|each| {
        for seq in 0..3 {
            each(0, tick(seq))?;
        }
        panic!("the read's own panic")
    });
    let mut taken = Vec::new();
    let raised = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        ahead(
            read,
            |entry| {
                taken.push(seq_of(&entry));
                Ok(())
            },
            |_| String::new(),
        )
    }));
    let payload = raised.expect_err("the read panicked");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"the read's own panic"));
    assert_eq!(taken, [0, 1, 2]);
}

/// An entry the caller refuses returns its error at once while the reader waits on a pipe whose writer
/// holds it open, as a read in place returns it before it reads again.
#[test]
fn an_entry_the_caller_refuses_returns_while_the_reader_waits_on_its_input() {
    let dir = scratch_dir("ahead-pipe");
    std::fs::create_dir_all(&dir).unwrap();
    let pipe = dir.join("log.pipe");
    let made = std::process::Command::new("mkfifo").arg(&pipe).status().expect("mkfifo runs");
    assert!(made.success(), "a named pipe");
    let (done, open) = std::sync::mpsc::channel::<()>();
    let writer = {
        let pipe = pipe.clone();
        std::thread::spawn(move || {
            let mut file = std::fs::OpenOptions::new().write(true).open(&pipe).expect("the pipe opens");
            let line = format!("{}\n", serde_json::to_string(&tick(0)).unwrap());
            std::io::Write::write_all(&mut file, line.as_bytes()).expect("the line is written");
            // Holds the pipe open, with nothing more on it, until the test is done.
            let _ = open.recv();
        })
    };
    let (outcome, returned) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let read = read_file_ahead(&pipe, |entry| anyhow::bail!("refused {}", seq_of(&entry)));
        let _ = outcome.send(read.map_err(|error| format!("{error:#}")));
    });
    let read =
        returned.recv_timeout(std::time::Duration::from_secs(30)).expect("the refusal returns while the pipe is open");
    drop(done);
    writer.join().unwrap();
    assert!(read.is_err_and(|error| error.ends_with("refused 0")));
}

/// An entry the read refuses ends it with the error an in-place read gives, once the caller has taken
/// every entry before it.
#[test]
fn an_entry_the_read_refuses_ends_it_with_the_in_place_error_after_every_earlier_entry() {
    let dir = scratch_dir("ahead-unreadable");
    write_log(&dir, COMMANDS);
    let file = dir.join("broken.jsonl");
    let mut lines = String::new();
    SegmentedLog::open(&dir, |entry| {
        lines.push_str(&format!("{}\n", serde_json::to_string(&entry).unwrap()));
        Ok(())
    })
    .unwrap();
    let broken = COMMANDS as usize / 2;
    let mut lines: Vec<&str> = lines.lines().collect();
    lines[broken] = "{not an entry";
    std::fs::write(&file, lines.join("\n") + "\n").unwrap();
    let (mut in_place, mut ahead) = (Vec::new(), Vec::new());
    let in_place_error = read_file(&file, |entry| {
        in_place.push(seq_of(&entry));
        Ok(())
    })
    .expect_err("a broken line");
    let ahead_error = read_file_ahead(&file, |entry| {
        ahead.push(seq_of(&entry));
        Ok(())
    })
    .expect_err("a broken line");
    assert_eq!(format!("{ahead_error:#}"), format!("{in_place_error:#}"));
    assert_eq!(ahead, in_place);
    assert_eq!(ahead.len(), broken);
}

// ── Helpers ──

/// A read of the log at a path that hands each entry to a visitor.
type Reading = fn(&Path, &mut dyn FnMut(LogEntry) -> anyhow::Result<()>) -> anyhow::Result<()>;

/// The seq of `entry`, a command.
fn seq_of(entry: &LogEntry) -> u64 {
    let LogEntry::Command(command) = entry else { panic!("the log holds commands alone") };
    command.sequenced.seq
}

/// A tick at `seq`, as its log entry.
fn tick(seq: u64) -> LogEntry {
    let sequenced = Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick };
    LogEntry::Command(CommandEntry { sequenced, events: Some(Vec::new()), seal: None })
}

/// A log of `count` ticks in `dir`, `SEGMENT` commands a segment.
fn write_log(dir: &Path, count: u64) {
    std::fs::create_dir_all(dir).unwrap();
    let log = SegmentedLog::open(dir, |_| Ok(())).unwrap();
    let mut writer = log.into_writer(SEGMENT, tokio::sync::watch::channel(Default::default()).0).unwrap();
    for seq in 0..count {
        writer.append(&tick(seq)).unwrap();
    }
    writer.flush().unwrap();
}
