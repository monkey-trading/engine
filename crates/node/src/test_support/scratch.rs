//! Scratch directories that go with the test that made them.
//!
//! `scratch_dir` returns the plain path, so a caller needs no guard to hold. The thread that asked for
//! it owns it: when that thread ends, which for a test is when the test has finished, the directory is
//! removed. A test whose thread panicked keeps its directories, so a failure can be inspected for as
//! long as the test process lives. `keep_scratch` keeps one on a passing test.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Once;

thread_local! {
    static OWNED: Owned = const { Owned { paths: RefCell::new(BTreeSet::new()) } };
    /// Set by the panic hook on the thread that panicked.
    static PANICKED: Cell<bool> = const { Cell::new(false) };
}

/// The directories one thread made, removed when it ends unless it panicked.
struct Owned {
    paths: RefCell<BTreeSet<PathBuf>>,
}

impl Drop for Owned {
    fn drop(&mut self) {
        if PANICKED.try_with(Cell::get).unwrap_or(false) {
            return;
        }
        for path in std::mem::take(&mut *self.paths.borrow_mut()) {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn mark_panics() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = PANICKED.try_with(|panicked| panicked.set(true));
            previous(info);
        }));
    });
}

/// The path `engine-node-test-<pid>-<name>`, owned by this thread from now on and left as it is.
pub fn scratch_path(name: &str) -> PathBuf {
    mark_panics();
    let dir = std::env::temp_dir().join(format!("engine-node-test-{}-{name}", std::process::id()));
    OWNED.with(|owned| owned.paths.borrow_mut().insert(dir.clone()));
    dir
}

/// An empty directory for one test, named after it, removed when the test's thread ends (see the
/// module). The directory is not created; the test makes it.
pub fn scratch_dir(name: &str) -> PathBuf {
    let dir = scratch_path(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Leaves `dir` where it is after this test, for a test that means to inspect it by hand.
pub fn keep_scratch(dir: &Path) {
    OWNED.with(|owned| owned.paths.borrow_mut().remove(dir));
}
