use super::*;
use crate::load_scenario::storage::Block;

#[test]
fn a_trader_gets_only_what_it_can_act_on_and_no_more_than_its_rate() {
    // Two clients of two traders each; only the second client's first trader rests an order.
    let holdings: Vec<Arc<std::sync::Mutex<Holding>>> = (0..4).map(|_| Arc::default()).collect();
    holdings[2].lock().unwrap().resting.push(7);
    let start = Instant::now();
    let slots = (0..4)
        .map(|at| Slot { client: at / 2, trader: at % 2, holding: holdings[at].clone(), free_at: start })
        .collect();
    let (channels, mut received): (Vec<_>, Vec<_>) = (0..2).map(|_| mpsc::unbounded_channel()).unzip();
    let in_flight = Arc::new(AtomicU64::new(0));
    let weights = vec![(2, 1)];
    let (clients, subscribers) = (Vec::new(), Vec::new());
    let mut scheduler = Scheduler {
        rng: Rng::new(1),
        slots,
        cursor: 0,
        weights,
        jobs: channels,
        in_flight,
        clients,
        subscribers,
        probe: None,
    };
    // Five requests a second among four traders, each free to send one every 1 / ACCOUNT_RATE.
    for step in 0..200 {
        scheduler.dispatch(start + Duration::from_millis(200 * step));
    }
    assert_eq!(scheduler.in_flight.load(Ordering::Acquire), 200);
    let mut last: BTreeMap<(usize, usize), Instant> = BTreeMap::new();
    let (mut cancels, mut markets) = (0, 0);
    for (client, jobs) in received.iter_mut().enumerate() {
        while let Ok(job) = jobs.try_recv() {
            if let Some(before) = last.insert((client, job.trader), job.due) {
                let spacing = job.due.duration_since(before).as_secs_f64();
                assert!(spacing >= 1.0 / ACCOUNT_RATE - 1e-9, "trader {client}/{} sent {spacing}s apart", job.trader);
            }
            match job.kind {
                Kind::Cancel | Kind::CancelAll => {
                    cancels += 1;
                    assert_eq!((client, job.trader), (1, 0), "only the trader resting an order cancels");
                }
                Kind::CloseAll => panic!("no trader holds a position to close"),
                Kind::Market => markets += 1,
                Kind::Limit => {}
            }
        }
    }
    assert!(cancels > 0 && markets > 0, "{cancels} cancels, {markets} market orders");
}

#[test]
fn the_first_reply_of_each_other_status_is_written_out_with_its_body() {
    let mut trader = traders::Trader::new(1);
    let body = serde_json::json!({ "error": "screening is at capacity" });
    let line = trader.refusal_line(503, &body).expect("the first 503 is said");
    assert!(line.contains("503") && line.contains("screening is at capacity"), "{line}");
    assert_eq!(trader.refusal_line(503, &body), None, "a second 503 is not");
    assert!(trader.refusal_line(429, &body).is_some(), "another status is");
}

#[tokio::test]
async fn a_leg_waits_for_every_subscriber_to_take_a_command_stamped_after_its_end() {
    let ended = 1_000;
    let subscriber = |newest: Option<i64>| Recorder::new(Samples { newest_stamp: newest, ..Samples::default() }.into());
    let scheduler = |subscribers| Scheduler {
        rng: Rng::new(1),
        slots: Vec::new(),
        cursor: 0,
        weights: Vec::new(),
        jobs: Vec::new(),
        in_flight: Arc::default(),
        clients: Vec::new(),
        subscribers,
        probe: None,
    };
    let caught_up = scheduler(vec![subscriber(Some(ended + 1)), subscriber(Some(ended + 250))]);
    let far = Instant::now() + Duration::from_secs(60);
    assert_eq!(caught_up.await_streams(ended, far).await, 0);
    // One took nothing since the window began, one only a command stamped at the leg's end.
    let behind = scheduler(vec![subscriber(Some(ended + 1)), subscriber(None), subscriber(Some(ended))]);
    let late = behind.subscribers[1].clone();
    let drained = Instant::now() + Duration::from_millis(500);
    let catching = tokio::spawn(async move { behind.await_streams(ended, drained).await });
    // The wait has looked once, with two subscribers behind, before the late one catches up.
    tokio::time::sleep(Duration::from_millis(50)).await;
    late.lock().unwrap().newest_stamp = Some(ended + 5);
    let left = tokio::time::timeout(Duration::from_secs(10), catching).await.expect("in time").expect("the task");
    assert_eq!(left, 1, "the subscriber that took a later command catches up; the one at the end does not");
}

/// A window's samples and the storage probe's blocks are cut together, each block read once, and a
/// leg's start drops the blocks from before it.
#[test]
fn a_window_closes_with_the_storage_probes_blocks_and_a_leg_starts_without_earlier_ones() {
    let since = Instant::now().checked_sub(Duration::from_secs(1)).expect("a clock a second on");
    let block = Block { blocked: Duration::from_millis(240), ..Block::default() };
    let probe = Probe::idle(since);
    probe.record(block, since + Duration::from_millis(1));
    let read = |probe: Option<&Probe>| close_window(&[], &[], probe).1.map(|storage| storage.block);
    assert_eq!(read(Some(&probe)), Some(block));
    assert_eq!(read(Some(&probe)), Some(Block::default()), "each block is read once");
    assert_eq!(read(None), None, "no probe, no reading");
    let probe = Probe::idle(since);
    probe.record(block, since + Duration::from_millis(1));
    let scheduler = Scheduler {
        rng: Rng::new(1),
        slots: Vec::new(),
        cursor: 0,
        weights: Vec::new(),
        jobs: Vec::new(),
        in_flight: Arc::default(),
        clients: Vec::new(),
        subscribers: Vec::new(),
        probe: Some(Arc::new(probe)),
    };
    scheduler.begin_leg();
    assert_eq!(read(scheduler.probe.as_deref()), Some(Block::default()), "a block from before the leg counts in none");
}

/// A block whose write ends while a window's samples are being taken counts in neither window, however
/// long the samples take after it: the orders and frames it held may have landed on either side.
#[test]
fn a_block_that_ends_while_a_windows_samples_are_taken_counts_in_neither_window() {
    let since = Instant::now().checked_sub(Duration::from_secs(1)).expect("a clock a second on");
    let probe = Arc::new(Probe::idle(since));
    let client = Recorder::new(Samples { order_to_fill_us: vec![9_000], ..Samples::default() }.into());
    let subscriber = Recorder::new(Samples { stream_lag_ms: vec![12], ..Samples::default() }.into());
    // The subscriber's recorder is held 300 ms, and a write of 240 ms ends 50 ms in, so the samples are
    // taken well over `MARGIN` after it.
    let (locked, holding) = std::sync::mpsc::channel();
    let (held, writing) = (subscriber.clone(), probe.clone());
    let holder = std::thread::spawn(move || {
        let _held = held.lock().expect("the subscriber's recorder");
        locked.send(()).expect("the window waits");
        std::thread::sleep(Duration::from_millis(50));
        writing.record(Block { blocked: Duration::from_millis(240), ..Block::default() }, Instant::now());
        std::thread::sleep(Duration::from_millis(250));
    });
    holding.recv().expect("the recorder held");
    let (samples, storage) =
        close_window(std::slice::from_ref(&client), std::slice::from_ref(&subscriber), Some(&probe));
    holder.join().expect("the holder");
    assert_eq!((samples.order_to_fill_us, samples.stream_lag_ms), (vec![9_000], vec![12]), "both recorders taken");
    assert_eq!(storage.map(|storage| storage.block), Some(Block::default()), "not in the window it ended in");
    let next = close_window(&[], &[], Some(&probe)).1.map(|storage| storage.block);
    assert_eq!(next, Some(Block::default()), "nor in the next");
}
