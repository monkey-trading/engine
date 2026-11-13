use super::*;

#[tokio::test(start_paused = true)]
async fn the_time_refresher_samples_every_second_and_ends_with_the_queue() {
    let (tx, mut rx) = mpsc::channel(8);
    let refresher = tokio::spawn(keep_time_fresh(tx.downgrade()));
    for _ in 0..3 {
        let sample = timeout(Duration::from_secs(2), rx.recv()).await.expect("a sample arrives each second");
        assert!(matches!(sample, Some(Request::TimeSample(_))));
    }
    assert!(rx.try_recv().is_err(), "one sample per second, no more");
    drop(tx);
    refresher.await.unwrap();
}

#[test]
fn a_scratch_directory_goes_when_the_thread_that_asked_for_it_ends() {
    let handle = std::thread::spawn(|| {
        let dir = scratch_dir("test-support-removed");
        std::fs::create_dir_all(dir.join("inner")).unwrap();
        std::fs::write(dir.join("inner/file"), b"x").unwrap();
        assert!(dir.exists());
        dir
    });
    let dir = handle.join().unwrap();
    assert!(!dir.exists(), "{} is left behind", dir.display());
    assert!(dir.to_string_lossy().contains(&format!("engine-node-test-{}-", std::process::id())));
}

#[test]
fn a_thread_that_panicked_keeps_its_scratch_directory_for_inspection() {
    let handle = std::thread::spawn(|| {
        let dir = scratch_dir("test-support-kept-on-failure");
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::panic::catch_unwind(|| panic!("a test fails"));
        dir
    });
    let dir = handle.join().unwrap();
    assert!(dir.exists(), "a failed test's directory must stay findable");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_scratch_directory_unwinding_from_a_panic_on_its_thread_stays() {
    let handle = std::thread::spawn(|| {
        let dir = scratch_dir("test-support-unwound");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), dir.display().to_string()).unwrap();
        panic!("{}", dir.display());
    });
    let payload = handle.join().unwrap_err();
    let dir = PathBuf::from(payload.downcast_ref::<String>().unwrap());
    assert!(dir.join("marker").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn keeping_a_scratch_directory_is_an_explicit_choice() {
    let handle = std::thread::spawn(|| {
        let dir = scratch_dir("test-support-opted-in");
        std::fs::create_dir_all(&dir).unwrap();
        keep_scratch(&dir);
        dir
    });
    let dir = handle.join().unwrap();
    assert!(dir.exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn asking_again_clears_the_directory_but_a_path_request_does_not() {
    let handle = std::thread::spawn(|| {
        let dir = scratch_dir("test-support-again");
        std::fs::create_dir_all(&dir).unwrap();
        let same = scratch_path("test-support-again");
        assert!(same.exists(), "a path request leaves the contents");
        assert_eq!(scratch_dir("test-support-again"), dir);
        assert!(!dir.exists(), "a directory request starts empty");
        dir
    });
    assert!(!handle.join().unwrap().exists());
}

/// The cleanup rests on libtest giving each test a thread of its own that ends with it, also with
/// `--test-threads=1`; on the main thread a directory would outlive its test.
#[test]
fn a_test_runs_on_a_thread_of_its_own() {
    let thread = std::thread::current();
    assert_ne!(thread.name(), Some("main"), "{thread:?}");
    assert!(thread.name().is_some_and(|name| name.ends_with("a_test_runs_on_a_thread_of_its_own")), "{thread:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_asked_for_in_a_multi_thread_test_body_outlives_its_workers() {
    let dir = scratch_dir("test-support-multi-thread");
    std::fs::create_dir_all(&dir).unwrap();
    let worker = tokio::spawn(async { tokio::task::yield_now().await });
    worker.await.unwrap();
    assert!(dir.exists());
}
