//! The scan of a log for its entries that are not commands (`log::uncommanded`): on any number of threads it
//! gives, in log order, the checkpoints and withdrawals a whole read gives.

use std::num::NonZeroUsize;

use super::*;

/// Across segments, on one thread or several, the scan gives the entries a whole read gives but the
/// commands, in log order, and leaves out a torn last line as a whole read does.
#[test]
fn the_scan_gives_a_whole_reads_entries_but_its_commands() {
    let dir = scratch_dir("scan-uncommanded");
    let entries = [cmd(0), checkpoint(0), withdrawal(1), cmd(1), cmd(2), checkpoint(2), cmd(3), withdrawal(2), cmd(4)];
    write(&dir, 2, &entries);
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
        assert_eq!(uncommanded(&dir, NonZeroUsize::new(jobs).unwrap()).unwrap(), whole, "{jobs} jobs");
    }
}

/// A line that is not a command's and not a valid entry fails the scan, naming its segment.
#[test]
fn the_scan_refuses_a_line_that_is_no_entry() {
    let dir = scratch_dir("scan-invalid");
    write(&dir, 2, &[cmd(0), checkpoint(0), cmd(1), cmd(2)]);
    append(&dir.join(SEG_2), b"{\"entry\":\"checkpoint\"}\n");
    let error = uncommanded(&dir, NonZeroUsize::MIN).expect_err("an invalid line");
    assert!(format!("{error:#}").contains(SEG_2), "{error:#}");
}
