//! The rollover's log entry from `V80` (#933): one aggregate per rollover, whose rows the sequencer
//! keeps for the history index and a replay of the log derives again, and a catch-up of missed
//! rollovers in one entry that holds no row per position.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use axum::extract::{Path as UrlPath, State};
use engine_core::financing::ChargesTree;
use engine_core::types::{MarketId, Side};
use prometheus_client::metrics::counter::Counter;
use tokio::sync::Semaphore;

use super::*;
use crate::api::{ApiState, Feed, MAX_BACKFILLS};
use crate::authenticated::Store;
use crate::clock::test_clock::TestClock;
use crate::config::HistorySection;
use crate::history::authenticated::Index;
use crate::history::derived::{DerivedRows, Deriver, RowsStore};
use crate::history::{Page, PageQuery};
use crate::log::{CheckpointDomain, CommandEntry, Manifest};
use crate::sequencer::history_rebuild::{rebuild_history_prefix, rebuild_history_view};
use crate::sequencer::margin_tests::{market_order, prices, sequenced};
use crate::sequencer::pipeline::{BATCH_CHARGE, MAX_IN_FLIGHT_BYTES};
use crate::sequencer::replay::LiveReplay;
use crate::test_support::{
    RETENTION, access, dev_config, log_entries, open_history, published_manifest, sample_around, scratch_dir,
    sequencer_config,
};

const BTC: MarketId = 1;
const ETH: MarketId = 2;
/// Tue 20 Oct 2026 10:00 New York.
const START: Millis = 1_792_504_800_000;
const DAY_MS: Millis = 24 * 3_600_000;
/// The positions #905's launch book holds (`Mix::proposal(100_000)`).
const LAUNCH_POSITIONS: u64 = 440_561;

/// A sequencer on the dev genesis with screened access, its log in `dir`, a minute before the
/// genesis's first rollover.
struct Rig {
    seq: Sequencer,
    clock: Arc<TestClock>,
    dir: PathBuf,
    manifest: watch::Receiver<Manifest>,
    _withdrawals: mpsc::UnboundedReceiver<SignedWithdrawal>,
}

#[test]
fn a_rollover_logs_one_aggregate_whose_rows_are_kept_and_derived_again_from_the_log() {
    let mut rig = Rig::new("rollover-aggregate");
    rig.open(2);
    let due = rig.seq.engine.next_rollover().expect("a rollover is due");
    rig.move_to(due + 100);
    let events = sequenced(&mut rig.seq, Command::Tick);
    let at = rig.seq.engine.last_seq().expect("a command");
    assert!(!events.iter().any(|e| matches!(e, Event::Financing { .. })), "no row per position is logged");
    let [(positions, accounts, root)] = aggregates(&events)[..] else { panic!("one aggregate: {events:?}") };
    assert_eq!((positions, accounts), (4, 2), "two accounts, each long BTC and short ETH");
    let kept = rig.seq.derived.borrow().get(at).cloned().expect("the sequencer keeps the rows");
    assert_eq!(kept.rows.len(), 4);
    assert_eq!(ChargesTree::new(&kept.rows).root(), root);
    rig.seq.flush().unwrap();

    // A history index reading the command back from the log with no rows kept derives them again,
    // and so does recovery's replay.
    let entries = log_entries(&rig.dir);
    let command = command_at(&entries, at);
    let gone = || watch::channel(DerivedRows::default()).1;
    let mut deriver = Deriver::new(gone(), Some((rig.dir.clone(), rig.manifest.clone())));
    assert_eq!(deriver.rows(&command).unwrap().as_deref(), Some(&*kept.rows));
    assert_eq!(deriver.replay_changes(), Some(0), "the replay keeps no record of changed keys");
    // A stopped history ends the replay, and its owner leaves the command to the next start; with no
    // stop, rows it cannot find are an error.
    let stop = Arc::new(AtomicBool::new(true));
    let mut stopped = Deriver::new(gone(), Some((rig.dir.clone(), rig.manifest.clone()))).stopped_by(stop);
    let err = stopped.rows(&command).expect_err("a stopped history");
    assert!(format!("{err:#}").contains("history stopped"), "{err:#}");
    assert!(stopped.taken(&command).unwrap().is_none(), "left to the next start");
    assert!(Deriver::new(gone(), None).taken(&command).is_err(), "no rows and no log");
    let taken = Deriver::new(rig.seq.derived_rows(), None).taken(&command).unwrap().expect("taken");
    assert_eq!((taken.command, taken.derived), (command.clone(), Some(kept.rows.clone())));
    let replayed = crate::sequencer::replay(&entries).unwrap();
    assert_eq!(replayed.derived.get(at).map(|derived| derived.rows.clone()), Some(kept.rows));
}

#[test]
fn a_client_order_past_a_due_rollover_replies_with_its_own_events_after_a_tick_that_carries_it() {
    let mut rig = Rig::new("rollover-before-order");
    rig.open(2);
    let due = rig.seq.engine.next_rollover().expect("a rollover is due");
    rig.move_to(due + 100);
    let events = market_order(&mut rig.seq, 1, BTC, Side::Buy, "0.01", "100000");
    let at = rig.seq.engine.last_seq().expect("a command");
    assert!(!events.is_empty() && aggregates(&events).is_empty(), "the reply holds only the order's: {events:?}");
    rig.seq.flush().unwrap();
    let entries = log_entries(&rig.dir);
    let (tick, order) = (command_at(&entries, at - 1), command_at(&entries, at));
    assert!(matches!(tick.sequenced.cmd, Command::Tick) && tick.sequenced.ts == order.sequenced.ts);
    assert_eq!(aggregates(tick.events.as_deref().unwrap()).len(), 1, "the tick carries the rollover");
    assert_eq!(order.events.as_deref(), Some(&events[..]));
    // The log replays to the sequencer's root.
    let mut replayed = crate::sequencer::replay(&entries).unwrap();
    assert_eq!(replayed.engine.state_root(), rig.seq.engine.state_root());
}

#[tokio::test]
async fn rows_gone_from_memory_come_from_the_store_while_they_give_the_logged_root() {
    let mut rig = Rig::new("rollover-stored");
    rig.open(2);
    let due = rig.seq.engine.next_rollover().expect("a rollover is due");
    rig.move_to(due + 100);
    sequenced(&mut rig.seq, Command::Tick);
    let at = rig.seq.engine.last_seq().expect("a command");
    let rows = rig.seq.derived.borrow().get(at).map(|derived| derived.rows.clone()).expect("kept");
    rig.seq.flush().unwrap();
    let command = command_at(&log_entries(&rig.dir), at);
    let (history, retention) = (scratch_dir("rollover-stored-history"), RETENTION.as_millis() as i64);
    let store = || Arc::new(RowsStore::new(&history, retention));
    let gone = || watch::channel(DerivedRows::default()).1;
    let deriver =
        |kept, replays| Deriver::new(kept, Some((rig.dir.clone(), rig.manifest.clone()))).storing(store(), replays);

    // The history stores the rows it takes from memory; once they are gone it takes them from there.
    assert_eq!(deriver(rig.seq.derived_rows(), Counter::default()).rows(&command).unwrap(), Some(rows.clone()));
    let replays = Counter::default();
    let mut stored = deriver(gone(), replays.clone());
    assert_eq!(stored.rows(&command).unwrap(), Some(rows.clone()));
    assert_eq!((stored.replay_changes(), replays.get()), (None, 0), "no replay");

    // The financing read proves an account's charges from them against the logged root.
    let (mut state, _requests) = crate::api::tests::api_state(true);
    (state.log_dir, state.manifest) = (rig.dir.clone(), rig.manifest.clone());
    state.history = state.history.with_derived(gone(), Some(store()));
    let read =
        |state: ApiState| crate::api::financing::financing(State(state), UrlPath((access::account(1).to_string(), at)));
    let view = read(state.clone()).await.expect("stored").0;
    let [(positions, _, root)] = aggregates(command.events.as_deref().unwrap())[..] else { panic!("one rollover") };
    let [rollover] = &view.rollovers[..] else { panic!("one rollover: {view:?}") };
    let proof = rollover.proof.as_ref().expect("account 1 is charged");
    assert!(rollover.root == root && proof.verify(root) && proof.charges.len() == 2, "{view:?}");
    assert_eq!(positions, 4);

    // Rows the host changed miss the logged root: the read answers none, and the history replays the
    // log, counts it, and stores the logged rows again.
    let mut changed = rows.to_vec();
    changed[0].amount += engine_core::Fixed::ONE;
    store().record(at, command.sequenced.ts, &changed).unwrap();
    state.history = state.history.with_derived(gone(), Some(store()));
    assert!(matches!(read(state.clone()).await, Err(crate::api::ApiError::NotFound)));
    let replays = Counter::default();
    assert_eq!(deriver(gone(), replays.clone()).rows(&command).unwrap(), Some(rows.clone()));
    assert_eq!(replays.get(), 1);
    assert_eq!(store().derived(&command).unwrap().map(|derived| derived.rows), Some(rows));
}

#[test]
fn three_missed_rollovers_log_one_entry_the_in_flight_bound_holds_at_any_book() {
    const HOLDERS: u8 = 6;
    let positions = u64::from(HOLDERS) * 2;
    let mut rig = Rig::new("missed-rollovers");
    rig.open(HOLDERS);
    let due = rig.seq.engine.next_rollover().expect("a rollover is due");
    // No command until past the third rollover: the next one catches all three up.
    rig.move_to(due + 2 * DAY_MS + 100);
    let events = sequenced(&mut rig.seq, Command::Tick);
    let at = rig.seq.engine.last_seq().expect("a command");
    let rows = rig.seq.derived.borrow().get(at).map(|derived| derived.rows.to_vec()).unwrap_or_default();
    rig.seq.flush().unwrap();
    let logged = command_at(&log_entries(&rig.dir), at);
    let bytes = line_bytes(&logged);
    assert_eq!(aggregates(&events).len(), 3, "one aggregate per rollover: {events:?}");
    assert_eq!(rows.len() as u64, 3 * positions, "every position's row at every rollover is derived");
    assert!(bytes < 4096, "the entry holds three aggregates, not a row per position: {bytes} bytes");
    assert!(bytes + BATCH_CHARGE <= MAX_IN_FLIGHT_BYTES);
    // Before `V80` the entry logs each rollover's rows in place of its aggregate, every other event
    // the same (`engine-core`'s `financing_aggregate`): at the launch book's positions, three
    // rollovers' entry is past the bound by itself.
    let mut rest = &rows[..];
    let earlier: Vec<Event> = logged
        .events
        .iter()
        .flatten()
        .flat_map(|event| match event {
            Event::FinancingApplied { positions, .. } => {
                let (own, after) = rest.split_at(usize::try_from(*positions).unwrap());
                rest = after;
                own.iter().map(|row| row.event()).collect()
            }
            event => vec![event.clone()],
        })
        .collect();
    let before = line_bytes(&CommandEntry { events: Some(earlier), ..logged });
    let per_row = (before - bytes) / (3 * positions);
    assert!(per_row * 3 * LAUNCH_POSITIONS > MAX_IN_FLIGHT_BYTES, "{per_row} bytes a row");
}

#[tokio::test]
async fn the_history_index_lists_a_rollovers_rows_kept_replayed_or_rebuilt() {
    let mut rig = Rig::new("rollover-history");
    rig.open(2);
    let due = rig.seq.engine.next_rollover().expect("a rollover is due");
    rig.move_to(due + 100);
    let logged = sequenced(&mut rig.seq, Command::Tick).len();
    let at = rig.seq.engine.last_seq().expect("a command");
    let rows = rig.seq.derived.borrow().get(at).map(|derived| derived.rows.clone()).expect("kept");
    rig.seq.flush().unwrap();
    // Account 1's rows, newest first, each past the command's logged events.
    let account = access::account(1);
    let mut expected: Vec<(String, Event)> = (logged..)
        .zip(rows.iter())
        .filter(|(_, row)| row.account == account)
        .map(|(index, row)| (format!("{at}:{index}"), row.event()))
        .collect();
    expected.reverse();
    assert_eq!(expected.len(), 2, "long BTC and short ETH");
    let query = || PageQuery { account, kind: Some("financing".into()), before: None, after: None, limit: 10 };
    let listed = |page: Page| page.events.into_iter().map(|e| (e.cursor.to_string(), e.event)).collect::<Vec<_>>();

    // The legacy index, taking the rows from memory, and a fresh one whose rows are gone, from a
    // replay of the log.
    for (name, kept) in [("kept", rig.seq.derived_rows()), ("replayed", watch::channel(DerivedRows::default()).1)] {
        let cfg =
            HistorySection { dir: scratch_dir(&format!("rollover-history-{name}")), retention: RETENTION, marks: None };
        let mut history = open_history(&cfg, &rig.dir).unwrap();
        history.catch_up_to(Some(at));
        let reader = history.reader();
        let (live, subscriber) = broadcast::channel(1);
        drop(live);
        let manifest = published_manifest(&rig.dir);
        let backfills = Arc::new(Semaphore::new(MAX_BACKFILLS));
        let feed = Feed::new(subscriber, rig.dir.clone(), manifest.clone(), backfills, Some(history.resume_at()));
        history.run(feed, Deriver::new(kept, Some((rig.dir.clone(), manifest)))).await.unwrap();
        assert_eq!(listed(reader.page(query()).await.unwrap().expect("caught up")), expected, "{name}");
    }

    // The authenticated index rebuilt from the log beside a strict replay.
    let rebuilt = scratch_dir("rollover-history-rebuilt");
    std::fs::create_dir_all(&rebuilt).unwrap();
    let store = Store::create(rebuilt.join("history.sqlite")).unwrap();
    let mut index = Index::empty(store, [0x33; 32], i64::try_from(RETENTION.as_millis()).unwrap()).unwrap();
    let replayed = crate::sequencer::replay(&log_entries(&rig.dir)).unwrap();
    let tip = (at, replayed.engine.now(), replayed.command_prefix);
    let rows = RowsStore::new(&rebuilt, RETENTION.as_millis() as i64);
    rebuild_history_prefix(&rig.dir, tip, &rows, &mut index, &AtomicBool::new(false)).unwrap();
    assert_eq!(listed(index.page(query()).unwrap()), expected, "rebuilt");
    // Each rebuild writes the rollover's rows file, which the financing read proves its rows from.
    let proves = |rows: &RowsStore| {
        let entries = log_entries(&rig.dir);
        let derived = rows.derived(&command_at(&entries, at)).unwrap().expect("a rows file");
        let [aggregate] = &derived.aggregates[..] else { panic!("one rollover") };
        ChargesTree::new(derived.rows_of(aggregate)).root() == aggregate.root
    };
    assert!(proves(&rows), "the prefix rebuild stored the rows");
    let strict = scratch_dir("rollover-history-strict");
    std::fs::create_dir_all(&strict).unwrap();
    let mut index = Index::empty(
        Store::create(strict.join("history.sqlite")).unwrap(),
        [0x34; 32],
        i64::try_from(RETENTION.as_millis()).unwrap(),
    )
    .unwrap();
    let mut fingerprint = crate::history::rebuild::Fingerprinter::new();
    log_entries(&rig.dir).iter().for_each(|entry| fingerprint.observe(entry).unwrap());
    let (view, rows) = (published_manifest(&rig.dir).borrow().clone(), RowsStore::new(&strict, 1_000_000));
    let expected = fingerprint.finish();
    rebuild_history_view(&rig.dir, view, &expected, &rows, &mut index).unwrap();
    assert!(proves(&rows), "the view rebuild stored the rows");
}

/// A rotation member's replica writes each rollover's rows to the history's store as it applies them, so its
/// index finds a rollover older than the replica's kept rows on disk when its turn comes.
#[test]
fn a_replica_stores_every_rollovers_rows_for_the_index_that_resumes_from_it() {
    const ROLLOVERS: usize = crate::history::derived::RETAINED + 2;
    let mut rig = Rig::new("rollover-replica");
    rig.open(2);
    let mut rollovers = Vec::new();
    for _ in 0..ROLLOVERS {
        let due = rig.seq.engine.next_rollover().expect("a rollover is due");
        rig.move_to(due + 100);
        sequenced(&mut rig.seq, Command::Tick);
        rollovers.push(rig.seq.engine.last_seq().expect("a command"));
    }
    rig.seq.flush().unwrap();
    let entries = log_entries(&rig.dir);
    let kept = rig.seq.derived.borrow().clone();
    assert!(kept.get(rollovers[0]).is_none(), "the first rollover's rows have left memory");

    let history = scratch_dir("rollover-replica-history");
    let store = Arc::new(RowsStore::new(&history, RETENTION.as_millis() as i64));
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
    let mut replica = LiveReplay::new(domain, [rig.signer()].into());
    replica.store_rows_in(store.clone());
    for entry in &entries {
        replica.apply(entry).unwrap();
    }
    // Every rollover's rows, the oldest included, are stored, and give the logged root.
    let rollovers_seqs = rollovers.clone();
    for seq in rollovers {
        let derived = store.derived(&command_at(&entries, seq)).unwrap().unwrap_or_else(|| panic!("no rows at {seq}"));
        let [aggregate] = &derived.aggregates[..] else { panic!("one rollover at {seq}") };
        assert_eq!(ChargesTree::new(derived.rows_of(aggregate)).root(), aggregate.root, "seq {seq}");
    }

    // A replica built again over files in place rewrites none; it writes a file that is missing or damaged.
    let replicate = || {
        let mut replica = LiveReplay::new(domain, [rig.signer()].into());
        replica.store_rows_in(store.clone());
        entries.iter().for_each(|entry| replica.apply(entry).unwrap());
    };
    let file = |seq: u64| history.join("financing").join(format!("{seq}.rows"));
    let identity = |seq: u64| {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(file(seq)).map(|meta| (meta.ino(), meta.mtime_nsec())).ok()
    };
    let (valid, missing, damaged) = (rollovers_seqs[0], rollovers_seqs[1], rollovers_seqs[2]);
    let before: Vec<_> = rollovers_seqs.iter().map(|seq| identity(*seq)).collect();
    std::fs::remove_file(file(missing)).unwrap();
    let mut bytes = std::fs::read(file(damaged)).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    std::fs::write(file(damaged), &bytes).unwrap();
    let damaged_identity = identity(damaged);
    replicate();
    assert_eq!(identity(valid), before[0], "a file holding the rows is not rewritten");
    assert!(identity(missing).is_some(), "a missing file is written");
    assert_ne!(identity(damaged), damaged_identity, "a damaged file is written again");
    for seq in &rollovers_seqs {
        assert!(store.derived(&command_at(&entries, *seq)).unwrap().is_some(), "rows stored at {seq}");
    }
}

/// A replica from a daily snapshot leaves the history's store the rows a whole replay would: the derivation through
/// the snapshot's command keeps each rollover's rows up to it, that command's included, none after it, and none once
/// stopped.
#[test]
fn the_rows_before_a_snapshot_are_derived_into_the_store() {
    let mut rig = Rig::new("rollover-derive-through");
    rig.open(2);
    let due = rig.seq.engine.next_rollover().expect("a rollover is due");
    rig.move_to(due + 100);
    sequenced(&mut rig.seq, Command::Tick);
    let at = rig.seq.engine.last_seq().expect("a command");
    sequenced(&mut rig.seq, Command::Tick);
    rig.seq.flush().unwrap();
    let rollover = command_at(&log_entries(&rig.dir), at);
    let store = |name: &str| RowsStore::new(&scratch_dir(name), RETENTION.as_millis() as i64);
    let (through, running) = (store("rollover-derive-through-at"), AtomicBool::new(false));
    assert_eq!(through.derive_through(&rig.dir, at, &running).unwrap(), Some(1), "one rollover through it");
    assert_eq!(through.derived(&rollover).unwrap().expect("its rows are kept").rows.len(), 4);
    let before = store("rollover-derive-through-before");
    assert_eq!(before.derive_through(&rig.dir, at - 1, &running).unwrap(), Some(0));
    assert!(before.derived(&rollover).unwrap().is_none(), "the rollover is after the snapshot's command");
    let stopped = store("rollover-derive-through-stopped");
    assert_eq!(stopped.derive_through(&rig.dir, at, &AtomicBool::new(true)).unwrap(), None);
    assert!(stopped.derived(&rollover).unwrap().is_none(), "a stopped derivation keeps nothing");
}

mod snapshots;

// ── Helpers ──

impl Rig {
    /// A sequencer at the latest version, past the dev genesis.
    fn new(name: &str) -> Self {
        let genesis = dev_config().genesis;
        let rollover = genesis.params.rollover.next_after(START);
        let clock = Arc::new(TestClock::at(rollover - 60_000));
        let dir = scratch_dir(name);
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let (withdrawals, _withdrawals) = mpsc::unbounded_channel();
        let cfg = SequencerConfig {
            clock: clock.clone(),
            genesis: genesis_commands(&genesis),
            access_policy: Some(access::policy()),
            ..sequencer_config(&dir, manifest_tx)
        };
        let key = Arc::new(EnclaveKey::test_key());
        let mut seq = Sequencer::recover(cfg, key, broadcast::channel(16).0, withdrawals).unwrap();
        seq.activate_protocol().unwrap();
        seq.genesis(&genesis).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        Self { seq, clock, dir, manifest, _withdrawals }
    }

    /// Accounts 1 to `holders`, each long 0.1 BTC and short 1 ETH.
    fn open(&mut self, holders: u8) {
        let now = self.clock.now_ms();
        sequenced(&mut self.seq, prices(BTC, "100000", now));
        sequenced(&mut self.seq, prices(ETH, "4000", now));
        for seed in 1..=holders {
            self.fund(seed);
            let long = market_order(&mut self.seq, seed, BTC, Side::Buy, "0.1", "100000");
            let short = market_order(&mut self.seq, seed, ETH, Side::Sell, "1", "4000");
            for events in [long, short] {
                assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
            }
        }
    }

    /// Declares account `seed` eligible and funds it with 100,000.
    fn fund(&mut self, seed: u8) {
        let declaration = access::declaration(&self.seq.engine, seed);
        let events = sequenced(&mut self.seq, declaration);
        assert!(matches!(events.as_slice(), [Event::EligibilityAttested { .. }]), "{events:?}");
        let (account, amount) = (access::account(seed), Fixed::from_int(100_000));
        let cmd = access::funding(&self.seq.engine, Some(account), Some(account), amount, &format!("dep-{seed}"));
        assert_eq!(sequenced(&mut self.seq, cmd), [Event::Deposited { account, amount }]);
    }

    /// The address of the key that seals the rig's commands.
    fn signer(&self) -> AccountId {
        AccountId(EnclaveKey::test_key().address())
    }

    /// Moves the clock to `ts` and takes a trusted time sample there.
    fn move_to(&mut self, ts: Millis) {
        self.clock.advance(Duration::from_millis(u64::try_from(ts - self.clock.now_ms()).unwrap()));
        self.seq.trust(sample_around(self.clock.as_ref())).unwrap();
    }
}

/// Each `financing_applied` among `events`: its positions, accounts and root.
fn aggregates(events: &[Event]) -> Vec<(u64, u64, engine_core::eip712::B256)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::FinancingApplied { positions, accounts, root, .. } => Some((*positions, *accounts, *root)),
            _ => None,
        })
        .collect()
}

/// The bytes of `command`'s log line, its newline included.
fn line_bytes(command: &CommandEntry) -> u64 {
    serde_json::to_vec(&LogEntry::Command(command.clone())).unwrap().len() as u64 + 1
}

/// The command the log in `entries` holds at `seq`.
fn command_at(entries: &[LogEntry], seq: u64) -> CommandEntry {
    entries
        .iter()
        .find_map(|entry| match entry {
            LogEntry::Command(command) if command.sequenced.seq == seq => Some(command.clone()),
            _ => None,
        })
        .expect("the log holds the command")
}
