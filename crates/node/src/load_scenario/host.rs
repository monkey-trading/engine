//! How long the host's tasks waited on storage and for a CPU, from Linux's pressure stall information
//! (`/proc/pressure`). The storage wait counts the node's own log syncs too, so a disk stall shows as a
//! window's wait standing well above its neighbours'.

use std::fmt;

/// The host's wait totals at one moment, in microseconds, each None where the host reports none.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Waits {
    pub(super) io_us: Option<u64>,
    pub(super) cpu_us: Option<u64>,
}

/// How long the host's tasks waited between two `Waits`, in milliseconds: on storage, the node's own
/// log syncs included, and for a CPU. Beside the batch time, a window that breaks its bar with far
/// more of either than its neighbours names the disk or the cores.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Waited {
    pub(super) io_ms: Option<f64>,
    pub(super) cpu_ms: Option<f64>,
}

/// `host_io_wait_ms=… host_cpu_wait_ms=…`, `n/a` where the host reports none.
impl fmt::Display for Waited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = |wait: Option<f64>| wait.map_or_else(|| "n/a".to_owned(), |ms| format!("{ms:.0}"));
        write!(f, "host_io_wait_ms={} host_cpu_wait_ms={}", ms(self.io_ms), ms(self.cpu_ms))
    }
}

impl Waits {
    /// The host's totals now.
    pub(super) fn now() -> Self {
        Waits { io_us: wait_us("io"), cpu_us: wait_us("cpu") }
    }

    /// How long the host's tasks waited from `earlier` to this.
    pub(super) fn since(&self, earlier: &Waits) -> Waited {
        let ms = |now: Option<u64>, then: Option<u64>| {
            now.zip(then).map(|(now, then)| now.saturating_sub(then) as f64 / 1_000.0)
        };
        Waited { io_ms: ms(self.io_us, earlier.io_us), cpu_ms: ms(self.cpu_us, earlier.cpu_us) }
    }
}

/// How long any task on the host has waited for `resource` (`io` or `cpu`), in microseconds.
fn wait_us(resource: &str) -> Option<u64> {
    std::fs::read_to_string(format!("/proc/pressure/{resource}")).ok().as_deref().and_then(some_total_us)
}

/// The `total` of the `some` line of a pressure stall file: the time at least one task stalled.
fn some_total_us(text: &str) -> Option<u64> {
    let some = text.lines().find(|line| line.starts_with("some "))?;
    some.split_whitespace().find_map(|field| field.strip_prefix("total="))?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_is_the_pressure_files_some_total() {
        let pressure = "some avg10=1.50 avg60=0.40 avg300=0.10 total=412345\n\
                        full avg10=1.20 avg60=0.30 avg300=0.08 total=365000\n";
        assert_eq!(some_total_us(pressure), Some(412_345), "the some line's total, not the full line's");
        assert_eq!(some_total_us("full avg10=0.00 avg60=0.00 avg300=0.00 total=9\n"), None);
        assert_eq!(some_total_us(""), None);
    }

    #[test]
    fn a_window_waited_what_each_total_grew_by_in_milliseconds_or_none_reported() {
        let (earlier, later) =
            (Waits { io_us: Some(1_000), cpu_us: Some(20_000) }, Waits { io_us: Some(251_000), cpu_us: Some(20_500) });
        let waited = later.since(&earlier);
        assert_eq!(waited, Waited { io_ms: Some(250.0), cpu_ms: Some(0.5) });
        assert_eq!(waited.to_string(), "host_io_wait_ms=250 host_cpu_wait_ms=0", "rounded");
        for (now, then) in [(later, Waits::default()), (Waits::default(), later)] {
            let waited = now.since(&then);
            assert_eq!(waited, Waited::default(), "a total the host did not report waits nothing");
            assert_eq!(waited.to_string(), "host_io_wait_ms=n/a host_cpu_wait_ms=n/a");
        }
    }
}
