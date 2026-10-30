//! The sequencer owns its OS thread; Linux workers exclude its reserved CPU.

use std::future::Future;

use anyhow::Context;
#[cfg(target_os = "linux")]
use tracing::info;
#[cfg(not(target_os = "linux"))]
use tracing::warn;

use tokio::sync::oneshot;

/// A supervised OS thread, cancelled at an async boundary whenever its supervisor drops it.
pub(crate) struct Worker {
    pub(crate) outcome: oneshot::Receiver<anyhow::Result<()>>,
    cancel: Option<oneshot::Sender<()>>,
}

impl Worker {
    pub(crate) fn start<F>(future: F, cpu: Option<usize>) -> anyhow::Result<Self>
    where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let runtime = tokio::runtime::Handle::current();
        let (done, outcome) = oneshot::channel();
        let (cancel, stopped) = oneshot::channel();
        std::thread::Builder::new()
            .name("sequencer".into())
            .spawn(move || {
                let result = pin(cpu).and_then(|()| {
                    runtime.block_on(async {
                        tokio::select! {
                            biased;
                            _ = stopped => anyhow::bail!("sequencer cancelled after supervisor stopped"),
                            result = future => result,
                        }
                    })
                });
                let _ = done.send(result);
            })
            .context("start sequencer thread")?;
        Ok(Self { outcome, cancel: Some(cancel) })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

/// Builds the node runtime after reserving a CPU, so its workers inherit the remaining mask.
/// Error cases:
/// - CPU reservation, runtime construction, node execution, or restoring the calling mask fails
pub fn run_dedicated(
    cfg: crate::NodeConfig,
    telemetry: crate::Telemetry,
    start: crate::KeyStart,
) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    let reservation = Reservation::new()?;
    #[cfg(target_os = "linux")]
    let cpu = Some(reservation.core);
    #[cfg(not(target_os = "linux"))]
    let cpu = None;
    let result = with_runtime(|runtime| runtime.block_on(crate::node::run_with_cpu(cfg, telemetry, cpu, start)));
    #[cfg(target_os = "linux")]
    reservation.restore()?;
    result
}

/// Stops runtime-owned tasks without waiting indefinitely for an uncancellable host syscall.
fn with_runtime(run: impl FnOnce(&tokio::runtime::Runtime) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = run(&runtime);
    runtime.shutdown_background();
    result
}

#[cfg(target_os = "linux")]
struct Reservation {
    original: nix::sched::CpuSet,
    core: usize,
}

#[cfg(target_os = "linux")]
impl Reservation {
    fn new() -> anyhow::Result<Self> {
        use nix::sched::{CpuSet, sched_getaffinity, sched_setaffinity};
        use nix::unistd::Pid;

        let original = sched_getaffinity(Pid::from_raw(0)).context("read node CPU mask")?;
        let cores: Vec<_> = (0..CpuSet::count()).filter(|cpu| original.is_set(*cpu).unwrap_or(false)).collect();
        anyhow::ensure!(cores.len() >= 2, "node requires at least two allowed CPUs to reserve the sequencer CPU");
        let core = *cores.last().expect("at least two CPUs");
        let mut workers = original;
        workers.unset(core).context("exclude sequencer CPU from workers")?;
        sched_setaffinity(Pid::from_raw(0), &workers).context("bind node workers away from sequencer CPU")?;
        Ok(Self { original, core })
    }

    fn restore(self) -> anyhow::Result<()> {
        nix::sched::sched_setaffinity(nix::unistd::Pid::from_raw(0), &self.original)
            .context("restore calling thread CPU mask")
    }
}

/// Pins the sequencer thread to the last CPU allowed by the Linux deployment's affinity mask.
/// Error cases:
/// - reading or setting Linux CPU affinity fails, or the allowed mask is empty
#[cfg(target_os = "linux")]
pub(crate) fn pin(cpu: Option<usize>) -> anyhow::Result<()> {
    use nix::sched::{CpuSet, sched_getaffinity, sched_setaffinity};
    use nix::unistd::Pid;

    let allowed = sched_getaffinity(Pid::from_raw(0)).context("read sequencer CPU affinity")?;
    let core = cpu
        .or_else(|| (0..CpuSet::count()).rev().find(|cpu| allowed.is_set(*cpu).unwrap_or(false)))
        .context("no CPU allowed for sequencer")?;
    let mut selected = CpuSet::new();
    selected.set(core).context("select sequencer CPU")?;
    sched_setaffinity(Pid::from_raw(0), &selected).context("pin sequencer CPU")?;
    info!(core, "sequencer pinned to CPU");
    Ok(())
}

/// Development platforms run a dedicated thread without a Linux CPU binding guarantee.
#[cfg(not(target_os = "linux"))]
pub(crate) fn pin(_cpu: Option<usize>) -> anyhow::Result<()> {
    warn!("sequencer has a dedicated thread; CPU pinning requires Linux");
    Ok(())
}

#[cfg(test)]
mod lifecycle_tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use tokio::sync::oneshot;

    struct Dropped(Option<oneshot::Sender<()>>);

    impl Drop for Dropped {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[tokio::test]
    async fn dropping_the_supervisor_cancels_its_worker_at_a_safe_async_boundary() {
        let (entered, ready) = oneshot::channel();
        let (dropped, finished) = oneshot::channel();
        let worker = super::Worker::start(
            async move {
                let _dropped = Dropped(Some(dropped));
                entered.send(()).unwrap();
                std::future::pending::<anyhow::Result<()>>().await
            },
            None,
        )
        .unwrap();
        ready.await.unwrap();
        drop(worker);
        tokio::time::timeout(Duration::from_secs(1), finished).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn worker_completion_and_panic_are_both_reported_to_the_supervisor() {
        let mut finished = super::Worker::start(async { Ok(()) }, None).unwrap();
        (&mut finished.outcome).await.unwrap().unwrap();
        let panic = std::future::poll_fn(|_| -> std::task::Poll<anyhow::Result<()>> {
            panic!("sequencer worker panic probe");
        });
        let mut failed = super::Worker::start(panic, None).unwrap();
        assert!((&mut failed.outcome).await.is_err());
    }

    #[test]
    fn fatal_runtime_teardown_returns_while_a_blocking_storage_task_is_stalled() {
        let (entered, ready) = mpsc::channel();
        let (running, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let (returned, result) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            let outcome = super::with_runtime(|runtime| {
                runtime.spawn_blocking(move || {
                    running.send(()).unwrap();
                    entered.send(()).unwrap();
                    released.recv().unwrap();
                    finished.send(()).unwrap();
                });
                started.recv_timeout(Duration::from_secs(5)).unwrap();
                Err(anyhow::anyhow!("sequencer did not drain in time"))
            });
            returned.send(outcome).unwrap();
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let returned_in_time = result.recv_timeout(Duration::from_secs(1));
        // Release the simulated syscall even when a teardown regression would wait for it.
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(5)).unwrap();
        owner.join().unwrap();
        assert_eq!(returned_in_time.unwrap().unwrap_err().to_string(), "sequencer did not drain in time");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn invalid_core_and_single_cpu_reservation_fail_without_corrupting_the_mask() {
        use nix::sched::{CpuSet, sched_getaffinity, sched_setaffinity};
        use nix::unistd::Pid;
        std::thread::spawn(|| {
            let original = sched_getaffinity(Pid::from_raw(0)).unwrap();
            assert!(super::pin(Some(CpuSet::count())).is_err());
            assert_eq!(sched_getaffinity(Pid::from_raw(0)).unwrap(), original);
            super::pin(None).unwrap();
            let single = sched_getaffinity(Pid::from_raw(0)).unwrap();
            assert!(super::Reservation::new().is_err());
            assert_eq!(sched_getaffinity(Pid::from_raw(0)).unwrap(), single);
            sched_setaffinity(Pid::from_raw(0), &original).unwrap();
        })
        .join()
        .unwrap();
    }

    #[test]
    fn reserved_cpu_is_excluded_from_runtime_workers_and_restored_after_shutdown() {
        use nix::sched::{CpuSet, sched_getaffinity};
        use nix::unistd::Pid;
        std::thread::spawn(|| {
            let original = sched_getaffinity(Pid::from_raw(0)).unwrap();
            let count = (0..CpuSet::count()).filter(|cpu| original.is_set(*cpu).unwrap()).count();
            if count < 2 {
                assert!(super::Reservation::new().is_err());
                return;
            }
            let reservation = super::Reservation::new().unwrap();
            assert!(!sched_getaffinity(Pid::from_raw(0)).unwrap().is_set(reservation.core).unwrap());
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .max_blocking_threads(1)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let worker = tokio::spawn(async { sched_getaffinity(Pid::from_raw(0)).unwrap() }).await.unwrap();
                assert!(!worker.is_set(reservation.core).unwrap());
                let core = reservation.core;
                let pool_before =
                    tokio::task::spawn_blocking(|| sched_getaffinity(Pid::from_raw(0)).unwrap()).await.unwrap();
                let (selected, observed) = tokio::sync::oneshot::channel();
                let mut owner = super::Worker::start(
                    async move {
                        selected.send(sched_getaffinity(Pid::from_raw(0)).unwrap()).unwrap();
                        Ok(())
                    },
                    Some(core),
                )
                .unwrap();
                let sequencer = observed.await.unwrap();
                (&mut owner.outcome).await.unwrap().unwrap();
                drop(owner);
                let pool_after =
                    tokio::task::spawn_blocking(|| sched_getaffinity(Pid::from_raw(0)).unwrap()).await.unwrap();
                assert_eq!(pool_before, pool_after, "sequencer affinity must not leak into the blocking pool");
                assert!(sequencer.is_set(core).unwrap());
                assert_eq!((0..CpuSet::count()).filter(|cpu| sequencer.is_set(*cpu).unwrap()).count(), 1);
            });
            drop(runtime);
            reservation.restore().unwrap();
            assert_eq!(sched_getaffinity(Pid::from_raw(0)).unwrap(), original);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn pinning_restricts_only_its_thread_to_one_previously_allowed_cpu() {
        use nix::sched::{CpuSet, sched_getaffinity};
        use nix::unistd::Pid;
        let parent = sched_getaffinity(Pid::from_raw(0)).unwrap();
        std::thread::spawn(|| {
            let before = sched_getaffinity(Pid::from_raw(0)).unwrap();
            super::pin(None).unwrap();
            let after = sched_getaffinity(Pid::from_raw(0)).unwrap();
            let selected: Vec<_> = (0..CpuSet::count()).filter(|cpu| after.is_set(*cpu).unwrap()).collect();
            assert_eq!(selected.len(), 1);
            assert!(before.is_set(selected[0]).unwrap());
        })
        .join()
        .unwrap();
        assert_eq!(sched_getaffinity(Pid::from_raw(0)).unwrap(), parent);
    }
}
