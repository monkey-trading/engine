//! Percentiles, resident memory and the clock the cases share.

use std::time::Instant;

/// The sorted `times` (microseconds) at `per_mille` thousandths, the sample at or above that share.
pub fn at(times: &[u128], per_mille: usize) -> u128 {
    times[(times.len() * per_mille).div_ceil(1000).clamp(1, times.len()) - 1]
}

/// Sorts `times` and prints one case: its samples and p50, p99 and p999 in microseconds, then `extra`.
/// Returns (p50, p99, p999).
pub fn report(name: &str, times: &mut [u128], extra: &str) -> (u128, u128, u128) {
    times.sort_unstable();
    let (p50, p99, p999) = (at(times, 500), at(times, 990), at(times, 999));
    println!("LAUNCH {name} samples={} p50_us={p50} p99_us={p99} p999_us={p999} {extra}", times.len());
    (p50, p99, p999)
}

/// Runs `work` and returns its result with the microseconds it took.
pub fn timed<T>(work: impl FnOnce() -> T) -> (T, u128) {
    let began = Instant::now();
    let out = work();
    (out, began.elapsed().as_micros())
}

/// This process's resident memory in MiB, from `ps`.
pub fn rss_mib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().expect("ps prints the resident size") >> 10
}

/// The budgets a case's figure exceeded, collected so every case prints before the test fails.
static OVER: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Holds `us` microseconds of `name` to `budget_us`, printing the verdict and noting an excess.
pub fn within(name: &str, us: u128, budget_us: u128) {
    let verdict = if us <= budget_us { "within" } else { "OVER" };
    println!("LAUNCH budget {name} us={us} budget_us={budget_us} {verdict}");
    if us > budget_us {
        OVER.lock().expect("budget notes").push(format!("{name} took {us}us against {budget_us}us"));
    }
}

/// The budgets exceeded so far.
pub fn exceeded() -> Vec<String> {
    OVER.lock().expect("budget notes").clone()
}
