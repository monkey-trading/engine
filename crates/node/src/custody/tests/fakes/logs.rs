//! Log lines captured from this thread's tasks, for the custody tests' alert assertions.

use std::sync::{Arc, Mutex};

use tracing::subscriber::DefaultGuard;

/// Log lines captured from this thread's tasks.
#[derive(Clone, Default)]
pub(in crate::custody::tests) struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    /// Captures every log line emitted on this thread until the guard drops.
    pub(in crate::custody::tests) fn capture() -> (Self, DefaultGuard) {
        crate::test_support::keep_callsites_interested();
        let logs = Logs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    pub(in crate::custody::tests) fn count(&self, needle: &str) -> usize {
        String::from_utf8_lossy(&self.0.lock().unwrap()).matches(needle).count()
    }

    /// The captured lines that contain `needle`.
    pub(in crate::custody::tests) fn lines_with(&self, needle: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|l| l.contains(needle))
            .map(str::to_owned)
            .collect()
    }
}
