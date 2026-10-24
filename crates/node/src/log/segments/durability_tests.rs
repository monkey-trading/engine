//! Failed durability cannot expose a withdrawal through the archive/stream publication boundary.

use super::*;
use engine_core::Fixed;
use engine_core::command::{Command, Sequenced};
use engine_core::types::AccountId;

use crate::log::CommandEntry;
use crate::sequencer::SignedWithdrawal;
use crate::test_support::scratch_dir;

#[test]
fn failed_sync_does_not_advance_stream_publication() {
    let dir = scratch_dir("withdrawal-sync-publication");
    let (published, visible) = watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(100, published).unwrap();
    writer
        .append(&LogEntry::Command(CommandEntry {
            sequenced: Sequenced { seq: 0, ts: 0, cmd: Command::Tick },
            events: Some(Vec::new()),
        }))
        .unwrap();
    writer.sync().unwrap();
    let before = visible.borrow().flushed;
    let released = writer
        .append(&LogEntry::Withdrawal(SignedWithdrawal {
            account: AccountId([1; 20]),
            amount: Fixed::ONE,
            nonce: 1,
            chain_id: 31337,
            vault: AccountId([2; 20]),
            signature: "0x".into(),
            approved_seq: None,
            authorization: None,
        }))
        .unwrap();
    // A character device accepts writes but cannot satisfy the required directory fsync.
    let directory = std::mem::replace(&mut writer.dir_handle, File::open("/dev/null").unwrap());
    assert!(writer.sync().is_err());
    assert_eq!(visible.borrow().flushed, before, "a failed commit must not expose the withdrawal");
    writer.dir_handle = directory;
    writer.sync().unwrap();
    assert_eq!(visible.borrow().flushed, Some(released));
}
