//! What a scenario reports of a child node's log when the child stops.

use std::path::Path;

/// How many of its log's last lines a child's exit reports: a node logs why it stopped last.
pub(super) const EXIT_LOG_LINES: usize = 40;

/// How many lines from a panic's message a child's exit reports: its backtrace, down to the frames of
/// the code that raised it, which sit below those of the panic machinery.
const PANIC_LOG_LINES: usize = 150;

/// What the log at `path` says of why its child stopped, or why it cannot be read: from the first
/// panic's message on, then, if the log goes on past that, its last `EXIT_LOG_LINES` lines; with no
/// panic, the last `EXIT_LOG_LINES` lines.
pub(super) fn log_tail(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => report(&String::from_utf8_lossy(&bytes)),
        Err(e) => format!("(the log cannot be read: {e})"),
    }
}

fn report(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let tail_from = lines.len().saturating_sub(EXIT_LOG_LINES);
    let Some(panic) = lines.iter().position(|line| line.contains("panicked at")) else {
        return lines[tail_from..].join("\n");
    };
    let panic_end = (panic + PANIC_LOG_LINES).min(lines.len());
    let mut report = lines[panic..panic_end].join("\n");
    if panic_end < lines.len() {
        let tail_from = tail_from.max(panic_end);
        report.push_str(&format!("\n[{} lines omitted]\n", tail_from - panic_end));
        report.push_str(&lines[tail_from..].join("\n"));
    }
    report
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{EXIT_LOG_LINES, PANIC_LOG_LINES, report};
    use crate::sequencer::withdrawal_scenario::node::ChildProcess;

    /// A panic's message and backtrace, as a test child's log holds them, among the lines around it.
    fn panicking_log(before: usize, after: usize) -> String {
        let before = (0..before).map(|n| format!("before {n}"));
        let panic = ["thread 'main' panicked at crates/node/src/start.rs:7:9:", "the key is missing"]
            .into_iter()
            .map(String::from);
        let frames =
            (0..60).map(|n| if n == 40 { "  40: engine_node::start::run".into() } else { format!("  {n}: frame") });
        let after = (0..after).map(|n| format!("after {n}"));
        before.chain(panic).chain(frames).chain(after).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn a_log_without_a_panic_reports_its_last_lines() {
        let log = (0..EXIT_LOG_LINES + 5).map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
        let expected = (5..EXIT_LOG_LINES + 5).map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
        assert_eq!(report(&log), expected);
    }

    #[test]
    fn a_panic_is_reported_whole_though_the_log_ends_in_its_backtrace() {
        let report = report(&panicking_log(500, 0));
        assert!(
            report.starts_with("thread 'main' panicked at crates/node/src/start.rs:7:9:\nthe key is missing"),
            "{report}"
        );
        assert!(report.contains("engine_node::start::run"), "the frame that raised it: {report}");
        assert!(!report.contains("before"), "{report}");
    }

    #[test]
    fn a_log_that_goes_on_after_a_panic_reports_it_and_the_end() {
        // Three lines, the 62 of the panic, and 200 after: the report holds the panic's first
        // PANIC_LOG_LINES lines and the last EXIT_LOG_LINES, the 72 between them omitted.
        let report = report(&panicking_log(3, 200));
        assert!(report.starts_with("thread 'main' panicked at"), "{report}");
        assert!(report.contains("\n[72 lines omitted]\nafter 160\n"), "{report}");
        assert!(report.ends_with("after 199"), "{report}");
        assert_eq!(report.lines().count(), PANIC_LOG_LINES + 1 + EXIT_LOG_LINES);
    }

    #[tokio::test]
    async fn a_child_that_panics_reports_the_panic_and_not_only_the_end_of_its_log() {
        let dir = crate::test_support::scratch_dir("child-panic-log");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("child.log");
        let script = format!(
            "echo started; echo \"thread 'main' panicked at crates/node/src/start.rs:7:9:\"; echo the key is missing; \
             echo '  40: engine_node::start::run'; seq 1 {}; exit 101",
            EXIT_LOG_LINES * 2
        );
        let mut child = ChildProcess::spawn("sh", &["-c", script.as_str()], &log, &[], None).unwrap();
        child.exited(Duration::from_secs(10)).await.unwrap();
        let error = format!("{:#}", child.check().unwrap_err());
        assert!(error.contains("exit status: 101"), "{error}");
        assert!(error.contains("panicked at crates/node/src/start.rs:7:9:\nthe key is missing"), "{error}");
        assert!(error.contains("engine_node::start::run"), "{error}");
    }
}
