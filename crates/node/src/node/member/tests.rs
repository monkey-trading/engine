//! A member's holdings: after a confirmed handover the incumbent goes back to following, a node that
//! is no member serves once, a shutdown while following ends the node, and the metrics listener
//! answers throughout.

use std::cell::RefCell;

use super::*;
use crate::test_support::dev_config;

/// Once a successor confirms the handover, the incumbent's run does not end the node: it waits for
/// its next turn again, following the operator in effect, which is now its successor.
#[tokio::test]
async fn after_a_confirmed_handover_the_incumbent_follows_again() {
    let calls = RefCell::new(Vec::new());
    let gave = Handed { generation: Some(3), reached: Some(1_000) };
    let mut ends = [Ended::HandedOver(gave), Ended::Stopped].into_iter();
    let wait = async |handed: Option<Handed>| {
        calls.borrow_mut().push(("follow", handed));
        Ok(Turn::Hold)
    };
    let serve = async |_start: Option<Box<Start>>, handed: Option<Handed>| {
        calls.borrow_mut().push(("serve", handed));
        Ok(ends.next().expect("served past the shutdown"))
    };
    run_turns(true, wait, serve).await.unwrap();
    // What the handover gave away reaches the next wait and the run after it.
    let after = Some(gave);
    assert_eq!(calls.into_inner(), [("follow", None), ("serve", None), ("follow", after), ("serve", after)]);
}

/// A node that is no member serves once, however its run ends, and never follows.
#[tokio::test]
async fn a_node_that_is_no_member_serves_once() {
    let served = RefCell::new(0);
    let wait = async |_: Option<Handed>| -> anyhow::Result<Turn> { panic!("a node that is no member followed") };
    let serve = async |_start: Option<Box<Start>>, _: Option<Handed>| {
        *served.borrow_mut() += 1;
        Ok(Ended::HandedOver(Handed { generation: None, reached: None }))
    };
    run_turns(false, wait, serve).await.unwrap();
    assert_eq!(served.into_inner(), 1);
}

/// A shutdown signal while the member follows ends the node without serving.
#[tokio::test]
async fn a_shutdown_while_following_ends_the_node() {
    let wait = async |_: Option<Handed>| Ok(Turn::Stop);
    let serve = async |_start: Option<Box<Start>>, _: Option<Handed>| -> anyhow::Result<Ended> {
        panic!("served after a shutdown")
    };
    run_turns(true, wait, serve).await.unwrap();
}

/// The metrics listener the process binds answers while the member follows, while it serves its turn,
/// and once it follows again after handing over.
#[tokio::test]
async fn metrics_answer_while_following_and_across_a_turn() {
    let listener = crate::metrics::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let url = format!("http://{}/metrics", listener.local_addr().unwrap());
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let scrape = async || client.get(&url).send().await.and_then(|answer| answer.error_for_status()).is_ok();
    let answered = RefCell::new(Vec::new());
    let gave = Handed { generation: Some(1), reached: None };
    let mut ends = [Ended::HandedOver(gave), Ended::Stopped].into_iter();
    let wait = async |_: Option<Handed>| {
        let ok = scrape().await;
        answered.borrow_mut().push(("follow", ok));
        Ok(Turn::Hold)
    };
    let serve = async |_start: Option<Box<Start>>, _: Option<Handed>| {
        let ok = scrape().await;
        answered.borrow_mut().push(("serve", ok));
        Ok(ends.next().expect("served past the shutdown"))
    };
    let metrics = Arc::new(crate::metrics::Metrics::new());
    crate::metrics::alongside(listener, metrics, run_turns(true, wait, serve)).await.unwrap();
    assert_eq!(answered.into_inner(), [("follow", true), ("serve", true), ("follow", true), ("serve", true)]);
}

/// The metrics listener answers while the member's own future holds its thread, as a replica's build
/// holds it while it replays: the server runs on a task of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_answer_while_the_members_future_holds_its_thread() {
    let listener = crate::metrics::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let held = async {
        let scrape = std::thread::spawn(move || scrape_blocking(addr));
        std::thread::sleep(std::time::Duration::from_millis(1_500));
        anyhow::Ok(scrape.join().unwrap())
    };
    let metrics = Arc::new(crate::metrics::Metrics::new());
    let answered = crate::metrics::alongside(listener, metrics, held).await.unwrap();
    assert!(answered, "no scrape was answered while the member's future held its thread");
}

/// Whether a plain HTTP scrape of `/metrics` at `addr` is answered with 200 within a second.
fn scrape_blocking(addr: std::net::SocketAddr) -> bool {
    use std::io::{Read, Write};
    let answer = (|| {
        let mut stream = std::net::TcpStream::connect(addr)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
        stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: metrics\r\nConnection: close\r\n\r\n")?;
        let mut head = [0; 12];
        stream.read_exact(&mut head)?;
        std::io::Result::Ok(head)
    })();
    answer.is_ok_and(|head| head.as_slice() == b"HTTP/1.1 200")
}

/// The process's own tasks, the seed relayer among them, run while the member follows, while it
/// serves its turn, and once it follows again after handing over, one instance for the process.
#[tokio::test]
async fn process_tasks_run_while_following_and_across_a_turn() {
    let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut tasks = crate::node::run::ProcessTasks::new();
    let counted = ticks.clone();
    tasks.spawn(async move {
        // As long as any test runs: the task outlives every holding below.
        for _ in 0..u32::MAX {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        ("ticker", Ok(()))
    });
    // Each holding sees the task tick on.
    let ticking = async || {
        let from = ticks.load(std::sync::atomic::Ordering::SeqCst);
        crate::test_support::wait_for("the process task to run", async {
            while ticks.load(std::sync::atomic::Ordering::SeqCst) <= from {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
    };
    let calls = RefCell::new(Vec::new());
    let gave = Handed { generation: Some(1), reached: None };
    let mut ends = [Ended::HandedOver(gave), Ended::Stopped].into_iter();
    let wait = async |_: Option<Handed>| {
        ticking().await;
        calls.borrow_mut().push("follow");
        Ok(Turn::Hold)
    };
    let serve = async |_start: Option<Box<Start>>, _: Option<Handed>| {
        ticking().await;
        calls.borrow_mut().push("serve");
        Ok(ends.next().expect("served past the shutdown"))
    };
    crate::node::run::supervised(&mut tasks, run_turns(true, wait, serve)).await.unwrap();
    assert_eq!(calls.into_inner(), ["follow", "serve", "follow", "serve"]);
    assert_eq!(tasks.len(), 1, "the process task was not kept across the turn");
}

/// A process task that stops ends the process, naming it.
#[tokio::test]
async fn a_process_task_that_stops_ends_the_process() {
    let mut tasks = crate::node::run::ProcessTasks::new();
    tasks.spawn(async { ("seed relayer", Err(anyhow::anyhow!("the relayer key was refused"))) });
    let forever = async { std::future::pending::<anyhow::Result<()>>().await };
    let err = crate::node::run::supervised(&mut tasks, forever).await.expect_err("the process outlived its task");
    assert!(format!("{err:#}").contains("the seed relayer stopped: the relayer key was refused"), "{err:#}");
}

/// A member rotates, seals its keys with a handover listener, and starts with no flag naming its keys.
#[test]
fn a_member_rotates_with_a_handover_listener_and_no_key_flag() {
    let mut cfg = dev_config();
    cfg.operator.rotation_enabled = true;
    cfg.tee.use_published_mock_key = false;
    cfg.tee.sealing = Some(toml::from_str("dir = 'keys'\n[handover]\nlisten = '127.0.0.1:9471'").unwrap());
    assert!(is_member(&cfg, &KeyStart::default()));
    assert!(!is_member(&cfg, &KeyStart { new_key: true, ..KeyStart::default() }), "--new-key names the keys");
    let predecessor = KeyStart { predecessor: Some("127.0.0.1:1".into()), ..KeyStart::default() };
    assert!(!is_member(&cfg, &predecessor), "--predecessor names the keys");
    cfg.tee.sealing = Some(toml::from_str("dir = 'keys'").unwrap());
    assert!(!is_member(&cfg, &KeyStart::default()), "a node with no handover listener is a member");
    cfg.operator.rotation_enabled = false;
    assert!(!is_member(&cfg, &KeyStart::default()), "a node with rotation off is a member");
}
