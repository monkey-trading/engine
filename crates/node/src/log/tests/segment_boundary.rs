//! A crash around a segment boundary: whatever it leaves, the writer takes no checkpoint as the
//! first entry of a segment, and the log opens again after every append that follows.

use super::*;

/// What a crash leaves in a log directory, given the torn start of the next segment's first command.
type Crash = fn(&Path, &[u8]);

/// A writer reopened on `dir`, with segments of two commands.
fn reopened(dir: &Path) -> LogWriter {
    SegmentedLog::open(dir, |_| Ok(())).unwrap().into_writer(2, watch::channel(Manifest::default()).0).unwrap()
}

/// Rotation lists a full segment, links the next one empty, and only then writes the next command.
/// A crash at each step leaves the log with its last command, seq 1, ending a full segment, and the
/// checkpoint a recovery owes that seq has nowhere to go but the segment before the boundary, which
/// takes it only while unlisted. Where it is listed, the checkpoint is refused and nothing is
/// written, so the log reads as before and goes on with the next command.
#[test]
fn a_checkpoint_is_never_the_first_entry_of_a_segment_a_crash_left() {
    let torn = line(&cmd(2))[..20].to_vec();
    let cases: [(&str, Crash, bool); 4] = [
        ("before the seal", |dir, _| write(dir, 2, &[cmd(0), cmd(1)]), true),
        (
            "listed, not linked",
            |dir, _| {
                write(dir, 2, &[cmd(0), cmd(1), cmd(2)]);
                fs::remove_file(dir.join(SEG_2)).unwrap();
            },
            false,
        ),
        (
            "linked, empty",
            |dir, _| {
                write(dir, 2, &[cmd(0), cmd(1), cmd(2)]);
                fs::write(dir.join(SEG_2), "").unwrap();
            },
            false,
        ),
        (
            "linked, torn",
            |dir, torn| {
                write(dir, 2, &[cmd(0), cmd(1), cmd(2)]);
                fs::write(dir.join(SEG_2), torn).unwrap();
            },
            false,
        ),
    ];
    for (name, crash, takes) in cases {
        let dir = scratch_dir(&format!("boundary-{}", name.replace([' ', ','], "-")));
        crash(&dir, &torn);
        let mut writer = reopened(&dir);
        assert_eq!(writer.takes_non_command(), takes, "{name}: the writer says whether the entry has a place");
        let taken = writer.append(&checkpoint(1)).is_ok();
        assert_eq!(taken, takes, "{name}");
        writer.flush().unwrap();
        drop(writer);
        let expected: Vec<LogEntry> = if takes { vec![cmd(0), cmd(1), checkpoint(1)] } else { vec![cmd(0), cmd(1)] };
        assert_eq!(log_entries(&dir), expected, "{name}: the log reads as it did");

        let mut writer = reopened(&dir);
        writer.append(&cmd(2)).unwrap();
        writer.append(&checkpoint(2)).unwrap();
        writer.flush().unwrap();
        drop(writer);
        let all: Vec<LogEntry> = [expected, vec![cmd(2), checkpoint(2)]].concat();
        assert_eq!(log_entries(&dir), all, "{name}: the log opens after the appends");
    }
}
