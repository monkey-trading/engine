//! The fixer's second call against the 256 blocks its first call's block hash can be recorded for.

use super::*;

/// A reader whose block is pinned when it is read, as a custody session's is: every call answers as
/// the chain stood then, after the chain's latency.
struct Pinned(Arc<FakeChain>);

struct PinnedBlock(ChainState);

impl StandbyChain for Pinned {
    type Block<'a>
        = PinnedBlock
    where
        Self: 'a;

    async fn latest(&self) -> anyhow::Result<PinnedBlock> {
        Ok(PinnedBlock(self.0.0.lock().unwrap().clone()))
    }

    async fn finalized(&self) -> anyhow::Result<PinnedBlock> {
        self.latest().await
    }
}

impl AgreedBlock for PinnedBlock {
    fn time(&self) -> u64 {
        self.0.time
    }

    fn number(&self) -> u64 {
        self.0.number
    }

    fn hash(&self) -> B256 {
        B256::from(U256::from(self.0.number))
    }

    async fn call(&self, _: &'static str, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        tokio::time::sleep(self.0.latency).await;
        crate::liveness::fake::answer(&self.0, to, &data).map(Bytes::from)
    }
}

/// Sends that take `delay` and note when each was done.
#[derive(Clone, Default)]
struct Timed(Arc<Mutex<Vec<tokio::time::Instant>>>, Duration);

impl Sending for Timed {
    async fn send(&self, _: Address, _: Vec<u8>) -> anyhow::Result<Option<B256>> {
        tokio::time::sleep(self.1).await;
        self.0.lock().unwrap().push(tokio::time::Instant::now());
        Ok(Some(B256::ZERO))
    }

    async fn balance(&self) -> anyhow::Result<U256> {
        Ok(U256::from(10u64.pow(18)))
    }
}

/// A draw's first fix lands a second after the fixer's read pinned the chain, which showed nothing
/// fixed, and no one makes the second call. With every call answering in 6.5 s, a read takes 26 s, and a
/// send 10 more: a fixer that waited its 30 s interval would send 66 s after the fix, past the 64 s the
/// block's hash can be recorded for. The interval shrinks with the slow poll, so its send is within.
#[tokio::test(start_paused = true)]
async fn the_second_fix_lands_inside_the_reach_when_every_custody_read_is_slow() {
    let latency = Duration::from_millis(6500);
    let (state, _) = pending(ChainState { latency, switch: false, ..Default::default() });
    let fixed = state.seed;
    let chain = Arc::new(FakeChain::new(ChainState { seed: (0, B256::ZERO, B256::ZERO, 0), ..state }));
    let sent = Timed(Arc::default(), Duration::from_secs(10));
    let mut fixer = fixer(Pinned(chain.clone()), sent.clone(), Arc::default());
    (fixer.idle, fixer.poll) = (Duration::from_secs(30), Duration::from_secs(5));
    let begun = tokio::time::Instant::now();
    let task = tokio::spawn(fixer.run());
    tokio::time::sleep(Duration::from_secs(1)).await;
    chain.set(|chain| chain.seed = (fixed.0, B256::ZERO, B256::ZERO, chain.number));
    tokio::time::sleep(Duration::from_secs(120)).await;
    task.abort();
    let first = sent.0.lock().unwrap().first().copied().expect("a fix is sent");
    let waited = first - begun - Duration::from_secs(1);
    assert!(waited <= REACH, "the second call lands {waited:?} after the first fix, past {REACH:?}");
}
