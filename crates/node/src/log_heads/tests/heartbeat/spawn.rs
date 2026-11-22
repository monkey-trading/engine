//! Spawning the poster under test.

use super::*;

/// `spawn_retrying`, its clock starting at true time `at.0` and its sample `at.1` milliseconds behind it.
pub(super) fn spawn_retrying_at(
    chain: FakeChain,
    keys: Option<HeldKeys>,
    retry: Duration,
    at: (Millis, Millis),
) -> Drive {
    spawn_sampled(chain, keys, retry, at, true).0
}

/// As `spawn_retrying_at`, with the sampler's sender, and no first sample yet unless `sampled`.
pub(super) fn spawn_sampled(
    chain: FakeChain,
    keys: Option<HeldKeys>,
    retry: Duration,
    at: (Millis, Millis),
    sampled: bool,
) -> (Drive, watch::Sender<Option<Sample>>) {
    let (true_ms, behind) = at;
    let (tip, rx) = watch::channel(Head::default());
    let (released, released_rx) = watch::channel(Released::default());
    let (signing, signing_rx) = watch::channel(Signing::Nothing);
    let (hold, hold_rx) = watch::channel(Hold::Live);
    let (signed_tx, signed) = watch::channel(None);
    let clock = Arc::new(TestClock::at(true_ms));
    let sample = Sample {
        at: crate::clock::Clock::instant(clock.as_ref()),
        earliest_ms: true_ms - behind - 3_000,
        latest_ms: true_ms - behind + 3_000,
    };
    let (samples, samples_rx) = watch::channel(sampled.then_some(sample));
    let trusted = TrustedClock::new(clock.clone(), samples_rx, Duration::from_secs(10));
    let gate = Gate::default();
    let heartbeat = Heartbeat {
        keys,
        signing: signing_rx,
        released: released_rx,
        hold: hold_rx,
        gate: gate.clone(),
        signed: signed_tx,
        trusted: trusted.clone(),
    };
    let poster = Poster {
        chain,
        key: Arc::new(crate::tee::EnclaveKey::test_key()),
        domain: CheckpointDomain { chain_id: 1, vault: AccountId([2; 20]) },
        interval: Duration::from_secs(3600),
        retry,
        freeze: crate::log_heads::FreezeLatch::default(),
        confirmed: None,
        anchored: None,
        trusted: Some(trusted.clone()),
        prefix_ring: Default::default(),
        stall_intervals: 2,
        heartbeat: Some(heartbeat),
    };
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let task = tokio::spawn(poster.run(rx, sequencer));
    (Drive { released, signing, hold, gate, signed, clock, trusted, tip, task }, samples)
}
