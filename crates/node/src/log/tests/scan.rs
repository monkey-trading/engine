//! The scan of a log for its entries that are not commands (`log::uncommanded`): on any number of threads it
//! gives, in log order, the checkpoints and withdrawals a whole read gives.

use std::num::NonZeroUsize;

use super::*;

/// Across segments, on one thread or several, the scan gives the entries a whole read gives but the
/// commands, in log order, leaves out a torn last line as a whole read does, and a command however written.
#[test]
fn the_scan_gives_a_whole_reads_entries_but_its_commands() {
    let dir = scratch_dir("scan-uncommanded");
    let entries = [cmd(0), checkpoint(0), withdrawal(1), cmd(1), cmd(2), checkpoint(2), cmd(3), withdrawal(2), cmd(4)];
    write(&dir, 2, &entries);
    // Blank lines, which a whole read passes over, and a torn last line, which it leaves out.
    append(&dir.join(SEG_4), b"\n \t\n");
    append(&dir.join(SEG_4), &line(&checkpoint(4))[..10]);
    let mut whole = Vec::new();
    SegmentedLog::open(&dir, |entry| {
        if command_seq(&entry).is_none() {
            whole.push(entry);
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(whole, [checkpoint(0), withdrawal(1), checkpoint(2), withdrawal(2)]);
    for jobs in [1, 3] {
        assert_eq!(uncommanded(&dir, NonZeroUsize::new(jobs).unwrap(), Some).unwrap(), whole, "{jobs} jobs");
    }
    // A command written with a leading space is parsed, and left out all the same.
    let open = dir.join(SEG_4);
    let held = std::fs::read(&open).unwrap();
    let cut = held.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    std::fs::write(&open, [b" ".as_slice(), &held[..cut]].concat()).unwrap();
    assert_eq!(uncommanded(&dir, NonZeroUsize::MIN, Some).unwrap(), whole);
}

/// A line that begins as a command's is a command's or no entry at all: its tag cannot appear twice, so the
/// scan passes it over without missing a checkpoint or a withdrawal.
#[test]
fn a_line_that_begins_as_a_commands_is_no_other_entry() {
    let LogEntry::Checkpoint(held) = checkpoint(0) else { unreachable!("a checkpoint entry") };
    let mut written = serde_json::to_value(&held).unwrap();
    written.as_object_mut().unwrap().remove("entry");
    let body = serde_json::to_string(&written).unwrap();
    let twice = format!(r#"{{"entry":"command","entry":"checkpoint",{}"#, &body[1..]);
    assert!(twice.as_bytes().starts_with(crate::log::scan::COMMAND_LINE));
    let error = serde_json::from_str::<LogEntry>(&twice).expect_err("a repeated tag");
    assert!(error.to_string().contains("duplicate field `entry`"), "{error}");
}

/// A line that is not a command's and not a valid entry fails the scan, naming its segment.
#[test]
fn the_scan_refuses_a_line_that_is_no_entry() {
    let dir = scratch_dir("scan-invalid");
    write(&dir, 2, &[cmd(0), checkpoint(0), cmd(1), cmd(2)]);
    append(&dir.join(SEG_2), b"{\"entry\":\"checkpoint\"}\n");
    let error = uncommanded(&dir, NonZeroUsize::MIN, Some).expect_err("an invalid line");
    assert!(format!("{error:#}").contains(SEG_2), "{error:#}");
}

/// A completed segment cut short, or with its last break dropped, as an interrupted download leaves it, fails
/// the scan, naming the file, where leaving out its last checkpoint would blame the signed chain.
#[test]
fn the_scan_names_a_damaged_completed_segment() {
    let dir = scratch_dir("scan-damaged");
    write(&dir, 2, &[cmd(0), cmd(1), checkpoint(1), cmd(2), cmd(3), cmd(4)]);
    let listed = std::fs::read(dir.join(SEG_0)).unwrap();
    for cut in [listed.len() - 1, listed.len() / 2] {
        std::fs::write(dir.join(SEG_0), &listed[..cut]).unwrap();
        let error = format!("{:#}", uncommanded(&dir, NonZeroUsize::MIN, Some).expect_err("a damaged segment"));
        assert!(error.contains(SEG_0) && error.contains("differs from"), "{error}");
    }
}
