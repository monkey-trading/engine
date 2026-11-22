//! Loopback ports a scenario names before its nodes bind them.

use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicU16, Ordering};

/// The first port a scenario's address may take, above the registered range's busiest part.
const FIRST: u16 = 20_000;

/// How many ports from `FIRST` a scenario's addresses are drawn from; the range ends below the 32768
/// where Linux starts handing out the source ports of outgoing connections.
const SPAN: u16 = 12_000;

/// A loopback address free now and unlikely to be taken before a node binds it.
///
/// An address from `127.0.0.1:0` is an ephemeral port, which an outgoing connection of the same
/// process (a chain's RPC client, a stream) can take as its source port in the time before a node binds
/// it, so the bind fails with "address in use". These addresses lie below the ephemeral range, where
/// only another listener can hold them, and each call moves on from the last, so one process never
/// names a port twice.
///
/// # Panics
/// - every port of the range is in use
pub fn quiet_addr() -> SocketAddr {
    static NEXT: AtomicU16 = AtomicU16::new(0);
    // A start that differs per process, so two test processes at once begin apart.
    let start = u16::try_from(std::process::id() % u32::from(SPAN)).expect("below SPAN");
    for _ in 0..SPAN {
        let step = NEXT.fetch_add(1, Ordering::Relaxed);
        let addr = SocketAddr::from(([127, 0, 0, 1], FIRST + (start + step) % SPAN));
        if TcpListener::bind(addr).is_ok() {
            return addr;
        }
    }
    panic!("no free port in {FIRST}..{}", FIRST + SPAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_lie_below_the_ephemeral_range_and_never_repeat() {
        let addrs: Vec<_> = (0..50).map(|_| quiet_addr()).collect();
        assert!(addrs.iter().all(|a| (FIRST..FIRST + SPAN).contains(&a.port())), "{addrs:?}");
        let distinct: std::collections::BTreeSet<_> = addrs.iter().collect();
        assert_eq!(distinct.len(), addrs.len());
    }
}
