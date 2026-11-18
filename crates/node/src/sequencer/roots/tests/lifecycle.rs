//! The worker's life beside the sequencer's: roots stay inline until `V9`'s first root hands the tree
//! over, a worker that dies stops the sequencer, and root work no part can take leaves it idle.

use engine_core::auth::ClientAuth;
use engine_core::command::AdminCommand;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;

use super::*;
use crate::sequencer::fixture_tests::replay_trusted;

const HANDED_OVER: &str = "state tree handed to the root worker";
const COMMITTED: &str = "state root committed";

#[tokio::test]
async fn a_fresh_log_roots_inline_before_v9_hands_the_tree_over_at_v9s_first_root_and_replays_root_for_root() {
    let dir = scratch_dir("roots-fresh-log");
    let clock = Arc::new(CountingClock::default());
    let cfg = SequencerConfig { checkpoint_every: 1, ..Node::config(&dir, &clock, None) };
    let key = Arc::new(EnclaveKey::test_key());
    let (withdrawals, signed) = mpsc::unbounded_channel();
    let mut seq = Sequencer::recover(cfg, key, broadcast::channel(64).0, withdrawals).expect("a fresh log");
    assert!(seq.engine.protocol_version() < ProtocolVersion::V9, "a fresh engine roots its whole encoding");
    seq.trust(sample_around(clock.as_ref())).expect("trusted time");
    let operator = seq.operator;
    let node = Node { seq, dir, clock, gate: None, signed };
    let running = node.run(Vec::new());

    running.submit(Command::Tick).await;
    running.submit(Command::Tick).await;
    // What `V9` needs recorded first.
    running.submit(Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned))).await;
    running.submit(Command::Admin(AdminCommand::SetOperator(operator))).await;
    let activation = Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V9 });
    let events = running.submit(activation).await;
    assert!(events.contains(&Event::AdminApplied), "V9 activates: {events:?}");
    running.submit(Command::Tick).await;
    running.submit(Command::Tick).await;
    let Stopped { ended, entries, logged } = running.stop().await;
    ended.expect("the sequencer drains");

    let replayed = replay_trusted(&entries).expect("every checkpoint's root is the replay's");
    assert_eq!((replayed.commands, replayed.checkpoints), (7, 7), "every command is checkpointed");
    assert_eq!(replayed.activations, [(ProtocolVersion::V9, 4)]);
    let roots: Vec<&Logged> = logged.iter().filter(|line| says(line, HANDED_OVER) || says(line, COMMITTED)).collect();
    assert_eq!(roots.len(), 3, "one hand-over, then two roots on the worker: {roots:?}");
    assert!(says(roots[0], HANDED_OVER) && roots[0]["version"] == "Some(4)", "V9's first root: {:?}", roots[0]);
    let committed: Vec<u64> = roots[1..].iter().map(|line| number(line, "seq")).collect();
    assert_eq!(committed, [5, 6], "the roots after it are the worker's");
}

#[tokio::test]
async fn a_worker_that_panics_stops_the_idle_sequencer() {
    let node = Node::new("roots-worker-panics", None);
    let panic = node.seq.roots.worker.panic_switch();
    let mut running = node.run(Vec::new());
    // A command answered: the loop runs, the tree is on the worker, and nothing more arrives.
    running.submit(Command::Tick).await;
    panic();

    let started = Instant::now();
    let ended = running.ended().await;
    let error = ended.expect_err("the sequencer stops with its worker");
    assert!(format!("{error:#}").contains("the state root worker stopped"), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(10), "it stops promptly, not at a deadline");
}

#[tokio::test]
async fn markets_alone_in_the_record_leave_the_sequencer_idle() {
    let mut node = Node::new("roots-markets-idle", None);
    let markets: Vec<engine_core::types::MarketId> = node.seq.engine.markets().keys().copied().collect();
    let ts = node.seq.engine.now();
    for market in &markets {
        let quotes = ["venue-a", "venue-b"].map(|source| SourceQuote {
            source: source.into(),
            mid: Fixed::from_int(100),
            ts,
            contract: None,
        });
        let cmd = Command::Prices { market: *market, quotes: quotes.to_vec() };
        node.seq.sequence(cmd).expect("the price applies").expect("trusted time admits it");
    }
    node.seq.flush().expect("the log flushes");
    // A bound whose copy floor one changed market is over: only a commit takes the markets, so no part
    // can bring them under it, and the sequencer must not keep copying empty parts out.
    node.seq.roots.set_bounds(4, Duration::from_secs(3_600));
    assert!(node.seq.engine.root_backlog() >= markets.len() && markets.len() > 1, "{markets:?}");
    let running = node.run(Vec::new());
    assert_eq!(running.batches_while_idle().await, 0, "the sequencer idles with only markets in the record");
    let Stopped { ended, .. } = running.stop().await;
    assert!(ended.is_ok(), "the sequencer drains: {ended:?}");
}
