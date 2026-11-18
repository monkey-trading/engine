use engine_core::Fixed;
use engine_core::command::{Command, Sequenced};
use engine_core::financing::ChargesTree;
use engine_core::types::AccountId;

use super::*;

#[test]
fn a_commands_rows_must_be_the_ones_its_aggregates_cover() {
    let events = [aggregate(2), Event::AdminApplied, aggregate(1)];
    let derived = Derived::of(7, &events, rows(3)).unwrap().expect("rows");
    let at = |aggregate: &Aggregate| (aggregate.index, aggregate.first, aggregate.positions);
    assert_eq!(derived.aggregates.iter().map(at).collect::<Vec<_>>(), [(0, 0, 2), (2, 2, 1)]);
    assert_eq!(derived.rows_of(&derived.aggregates[1]), &rows(3)[2..]);
    for (events, rows) in [(vec![aggregate(2)], rows(3)), (vec![aggregate(2)], rows(1)), (Vec::new(), rows(1))] {
        assert!(Derived::of(7, &events, rows).is_err(), "rows the aggregates do not cover are refused");
    }
    assert!(Derived::of(7, &[Event::AdminApplied], Vec::new()).unwrap().is_none(), "no rows, nothing kept");
}

#[test]
fn the_newest_commands_rows_are_kept_and_each_rollovers_tree_proves_its_rows() {
    let mut kept = DerivedRows::default();
    let seqs: Vec<u64> = (0..=RETAINED as u64).map(|n| 3 + 3 * n).collect();
    for &seq in &seqs {
        kept.keep(seq, Derived::of(seq, &[aggregate(4)], rows(4)).unwrap().expect("rows"));
    }
    assert!(kept.get(seqs[0]).is_none(), "the oldest past {RETAINED} goes");
    assert!(kept.get(seqs[1]).is_some(), "the next is kept");
    let derived = kept.get(seqs[RETAINED]).expect("the newest is kept");
    assert_eq!(derived.rows.len(), 4);
    let [tree] = derived.trees() else { panic!("one rollover's tree") };
    let proof = tree.proof(&derived.rows, account(2)).expect("a charged account");
    assert!(proof.verify(ChargesTree::new(&rows(4)).root()));
    assert_eq!(proof.charges.len(), 2, "both of the account's charges");
    assert!(std::ptr::eq(derived.trees(), derived.trees()), "the trees are built once");
}

#[test]
fn a_deriver_takes_kept_rows_and_none_for_a_command_that_derives_none() {
    let (sender, receiver) = watch::channel(DerivedRows::default());
    let mut deriver = Deriver::new(receiver, None);
    assert_eq!(deriver.rows(&command(5, vec![Event::AdminApplied])).unwrap(), None);
    let rollover = command(6, vec![aggregate(4)]);
    let err = deriver.rows(&rollover).expect_err("no rows kept and no log to replay");
    assert!(format!("{err:#}").contains("no rows kept for seq 6"), "{err:#}");
    sender.send_modify(|kept| kept.keep(6, Derived::of(6, &[aggregate(4)], rows(4)).unwrap().expect("rows")));
    assert_eq!(deriver.rows(&rollover).unwrap().as_deref(), Some(&rows(4)));
    let short = command(6, vec![aggregate(3)]);
    assert!(deriver.rows(&short).is_err(), "kept rows other than the aggregates cover are refused");
}

#[tokio::test]
async fn the_legacy_index_stores_derived_rows_after_the_logged_events() {
    let path = crate::test_support::scratch_dir("derived-legacy").join(crate::history::INDEX_FILE);
    let retention = crate::test_support::RETENTION.as_millis() as i64;
    let mut writer = crate::history::store::Writer::open(&path, retention).unwrap().0;
    let logged = command(6, vec![aggregate(4), Event::Deposited { account: account(2), amount: Fixed::ONE }]);
    writer.write_derived(&[(&logged, Some(Arc::new(rows(4))))]).unwrap();
    let reader = crate::history::Reader::new(path, watch::channel(true).1);
    let query = |kind: Option<&str>| crate::history::PageQuery {
        account: account(2),
        kind: kind.map(str::to_owned),
        before: None,
        after: None,
        limit: 10,
    };
    let page = reader.page(query(Some("financing"))).await.unwrap().expect("ready");
    let cursors: Vec<String> = page.events.iter().map(|event| event.cursor.to_string()).collect();
    assert_eq!(cursors, ["6:4", "6:3"], "account 2's two rows, past the two logged events");
    assert_eq!(page.events[1].event, rows(4)[1].event(), "the event the row stands for");
    let all = reader.page(query(None)).await.unwrap().expect("ready");
    assert_eq!(all.events.len(), 3, "the deposit and the rows");
}

#[test]
fn stored_rows_are_taken_only_once_they_give_the_logged_aggregates() {
    let store = RowsStore::new(&crate::test_support::scratch_dir("derived-store"), 1_000);
    let rows = rows(4);
    let logged = command(6, vec![rooted(&rows)]);
    assert!(store.derived(&logged).unwrap().is_none(), "no file");
    store.record(6, 0, &rows).unwrap();
    assert_eq!(store.derived(&logged).unwrap().expect("stored").rows.as_slice(), rows);
    let mut changed = rows.clone();
    changed[1].amount = Fixed::ONE;
    store.record(6, 0, &changed).unwrap();
    assert!(store.derived(&logged).is_err(), "rows the host changed miss the logged root");
    store.record(6, 5, &rows).unwrap();
    assert!(store.derived(&logged).is_err(), "another command's rows");
    store.record(6, 0, &rows[..3]).unwrap();
    assert!(store.derived(&logged).is_err(), "another count of rows");
    assert!(store.derived(&command(7, vec![Event::AdminApplied])).unwrap().is_none(), "a command with no rows");
}

#[test]
fn the_store_keeps_rows_for_the_retention_and_drops_what_a_crash_or_the_host_left() {
    let dir = crate::test_support::scratch_dir("derived-store-prune");
    let (store, financing) = (RowsStore::new(&dir, 1_000), dir.join("financing"));
    store.record(1, 0, &rows(2)).unwrap();
    for (name, bytes) in [("7.partial", &b"crash"[..]), ("8.rows", b"not rows"), ("notes.txt", b"other")] {
        std::fs::write(financing.join(name), bytes).unwrap();
    }
    store.record(2, 1_000, &rows(2)).unwrap();
    assert!(financing.join("1.rows").exists(), "a command within the retention");
    assert!(!financing.join("7.partial").exists() && !financing.join("8.rows").exists());
    store.record(3, 1_001, &rows(2)).unwrap();
    assert!(!financing.join("1.rows").exists(), "a command past the retention");
    assert!(financing.join("2.rows").exists() && financing.join("notes.txt").exists());
}

// ── Helpers ──

fn account(seed: u8) -> AccountId {
    AccountId([seed; 20])
}

/// A rollover's aggregate over `positions` rows, its root not checked here.
fn aggregate(positions: u64) -> Event {
    Event::FinancingApplied { at: 0, span: 0, positions, accounts: 0, net: Fixed::ZERO, root: B256::ZERO }
}

/// A rollover's aggregate over `rows`, with their root.
fn rooted(rows: &[FinancingRow]) -> Event {
    let (positions, root) = (rows.len() as u64, ChargesTree::new(rows).root());
    Event::FinancingApplied { at: 0, span: 0, positions, accounts: 0, net: Fixed::ZERO, root }
}

/// `count` rows over accounts 1, 2, 2, 3, ..., each in its own market, in a rollover's order.
fn rows(count: u32) -> Vec<FinancingRow> {
    (0..count)
        .map(|market| FinancingRow {
            account: account(u8::try_from(market.div_ceil(2) + 1).unwrap()),
            market,
            amount: Fixed::from_int(i64::from(market) - 1),
        })
        .collect()
}

fn command(seq: u64, events: Vec<Event>) -> CommandEntry {
    CommandEntry { sequenced: Sequenced { seq, ts: 0, cmd: Command::Tick }, events: Some(events), seal: None }
}
