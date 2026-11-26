//! A production build refuses a log directory that shares a device with the stores whose writes stall its syncs; a
//! development build warns.

use std::io;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::subscriber::with_default;
use tracing_subscriber::fmt::MakeWriter;

use super::*;
use crate::config::{DailySnapshotsSection, SnapshotSection};

use crate::test_support::DEV_CONFIG as DEV;

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

fn config() -> NodeConfig {
    // Parsed, not loaded: a production build's load refuses the dev config's providers and origins first.
    let mut cfg: NodeConfig = toml::from_str(&std::fs::read_to_string(DEV).unwrap()).unwrap();
    cfg.snapshots = Some(SnapshotSection {
        dir: "data/dev/snapshots".into(),
        retain: NonZeroUsize::MIN,
        interval: Duration::from_secs(3600),
        max_bytes: NonZeroUsize::MIN,
        trusted_signers: Vec::new(),
    });
    cfg.daily_snapshots = Some(DailySnapshotsSection::at("data/dev/daily-snapshots".into()));
    cfg
}

/// Each of the three stores is refused by name when it is on the log's device, and none is when only the log is.
#[test]
fn a_production_build_refuses_each_store_on_the_logs_device() {
    let cfg = config();
    let (log, history, snapshots, daily) = (&cfg.node.log_dir, &cfg.history.dir, "snapshots", "daily-snapshots");
    let device_of = |own: &'static str| {
        move |path: &Path| -> anyhow::Result<u64> {
            let own_dir = path.ends_with(own) || (own == "history" && path == history);
            Ok(if path == log || own_dir { 1 } else { 2 })
        }
    };
    for (name, own) in [("history.dir", "history"), ("snapshots.dir", snapshots), ("daily_snapshots.dir", daily)] {
        let shared = cfg.stores_on_the_logs_volume(device_of(own)).unwrap();
        assert_eq!(shared.iter().map(|(name, _)| *name).collect::<Vec<_>>(), [name]);
    }
    assert!(cfg.stores_on_the_logs_volume(device_of("none")).unwrap().is_empty());
}

/// The scratch directories share one disk: a production build refuses, naming the log and each store; a development
/// build loads and warns; a log on a device of its own passes.
#[test]
fn a_shared_volume_is_refused_in_production_and_warned_about_in_development() {
    let cfg = config();
    let refused = format!("{:#}", cfg.check_log_volume(true).unwrap_err());
    for named in ["node.log_dir", "history.dir", "snapshots.dir", "daily_snapshots.dir"] {
        assert!(refused.contains(named), "{named}: {refused}");
    }
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt().with_writer(captured.clone()).with_ansi(false).finish();
    with_default(subscriber, || cfg.check_log_volume(false).unwrap());
    let logged = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(logged.contains("WARN") && logged.contains("history.dir"), "{logged}");

    // A directory not yet made is on its nearest existing ancestor's device, through a link as well.
    let root = crate::test_support::scratch_dir("log-volume");
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&root, root.join("alias")).unwrap();
    assert_eq!(device(&root.join("alias/not/yet/made")).unwrap(), device(&root).unwrap());
}
