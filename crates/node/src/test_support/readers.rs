//! Stand-ins for the readers an `ApiState` holds that a test does not read.

use crate::test_support::{dev_config, scratch_dir};

/// A marks-cache reader over a scratch store no test reads; one directory per calling thread.
pub fn marks_reader() -> crate::history::marks::Reader {
    use crate::history::marks::{Retentions, Store};
    let dir = scratch_dir(&format!("unused-marks-{:?}", std::thread::current().id()));
    Store::open_in(&dir, Retentions::defaults(), [0x42; 32]).expect("scratch").reader()
}

/// An empty dashboard record under the dev config.
pub fn stats() -> crate::stats::Stats {
    crate::stats::Stats::new(&dev_config()).expect("the dev config's stats settings")
}
