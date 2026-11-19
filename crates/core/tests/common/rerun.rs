//! A latency guard's second chance (#1085). On a runner whose figures are all several times their
//! usual size a guard breaks on unchanged code; a real regression breaks it again.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

/// Runs `measure`, a benchmark's measurement and its bars' assertions. If it panics, prints that, runs
/// it once more and fails only if the second run panics too, with the second run's panic. Each run
/// prints its own figures, so both are in the log; the bars are the measurement's own.
pub fn rerun_once<R>(name: &str, mut measure: impl FnMut() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(&mut measure)) {
        Ok(done) => done,
        Err(first) => {
            println!("LATENCY rerun {name}: the first run broke a bar ({}); measuring it once more", message(&*first));
            match catch_unwind(AssertUnwindSafe(&mut measure)) {
                Ok(done) => {
                    println!("LATENCY rerun {name}: the second run held every bar");
                    done
                }
                Err(second) => {
                    println!("LATENCY rerun {name}: the second run broke a bar too ({})", message(&*second));
                    resume_unwind(second)
                }
            }
        }
    }
}

/// What a panic said.
fn message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}
