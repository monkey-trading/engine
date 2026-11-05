//! A fake chain for the poster's tests: the posted head, epoch, and failures the test drives,
//! enforcing the contract's chaining rule (a post must extend the held head of its epoch and
//! beat its seq) so the poster's parent tracking is tested against the rule it answers to.

use std::sync::{Arc, Mutex};

use crate::log_heads::{Head, HeadsChain, PostOutcome};

#[derive(Default)]
struct FakeState {
    epoch: u64,
    /// The chain's held head, and the epoch it belongs to: the contract keeps one head per
    /// signer epoch, so a rotation starts from an empty head even while an older epoch holds one.
    posted: Option<Head>,
    posted_epoch: u64,
    posts: Vec<(u64, Option<Head>, Head)>,
    fail_reads: usize,
    /// Every quorum read the poster made.
    reads: usize,
    /// Sends whose response is lost, as a timeout after broadcasting.
    fail_posts: usize,
    post_attempts: usize,
    /// Answer sends with `Waiting`: a pending reservation inside its bounded wait.
    waiting: bool,
    /// Accept sends without including them: the endpoint acknowledges but the chain head never
    /// advances.
    drop_after_accept: bool,
    /// Accept sends but hold them back from the chain: the endpoint acknowledges, and the quorum
    /// still sees the older head until the test includes them.
    hold: bool,
}

/// A chain whose posted head, epoch, and read failures the test drives. `post` enforces the
/// contract's chaining rule — a post must extend the held head of its epoch exactly, and beat its
/// seq — so the poster's parent tracking is tested against the rule it answers to.
#[derive(Clone, Default)]
pub(super) struct FakeChain(Arc<Mutex<FakeState>>);

impl FakeChain {
    pub(super) fn new(posted: Option<Head>) -> Self {
        let chain = Self::default();
        {
            let mut state = chain.0.lock().unwrap();
            state.epoch = 1;
            state.posted = posted;
            state.posted_epoch = 1;
        }
        chain
    }

    pub(super) fn posts(&self) -> Vec<(u64, Head)> {
        self.0.lock().unwrap().posts.iter().map(|(epoch, _, head)| (*epoch, *head)).collect()
    }

    /// The posts with the parent each extended.
    pub(super) fn posts_with_parents(&self) -> Vec<(u64, Option<Head>, Head)> {
        self.0.lock().unwrap().posts.clone()
    }

    pub(super) fn post_attempts(&self) -> usize {
        self.0.lock().unwrap().post_attempts
    }

    pub(super) fn reads(&self) -> usize {
        self.0.lock().unwrap().reads
    }

    pub(super) fn fail_next_reads(&self, count: usize) {
        self.0.lock().unwrap().fail_reads = count;
    }

    pub(super) fn fail_next_posts(&self, count: usize) {
        self.0.lock().unwrap().fail_posts = count;
    }

    /// Answers every send with `Waiting`, as a pending reconciliation inside its bound would.
    pub(super) fn wait_posts(&self) {
        self.0.lock().unwrap().waiting = true;
    }

    /// Acknowledge every send without advancing the chain head, as an endpoint that drops the
    /// transaction from its mempool after accepting it would.
    pub(super) fn drop_after_accept(&self) {
        self.0.lock().unwrap().drop_after_accept = true;
    }

    /// Acknowledge every send but hold it back from the chain, as slow inclusion would: the
    /// quorum keeps seeing the older head until the test calls `set_posted`.
    pub(super) fn hold_posts(&self) {
        self.0.lock().unwrap().hold = true;
    }

    pub(super) fn set_epoch(&self, epoch: u64) {
        self.0.lock().unwrap().epoch = epoch;
    }

    /// The quorum comes to see `posted` for the current epoch, as inclusion or another relayer's
    /// copy landing would.
    pub(super) fn set_posted(&self, posted: Head) {
        let mut state = self.0.lock().unwrap();
        state.posted_epoch = state.epoch;
        state.posted = Some(posted);
    }

    /// The reorg removes the epoch's head entirely: the quorum sees an empty epoch.
    pub(super) fn clear_posted(&self) {
        self.0.lock().unwrap().posted = None;
    }
}

impl HeadsChain for FakeChain {
    async fn posted(&self) -> anyhow::Result<(u64, Option<Head>)> {
        let mut state = self.0.lock().unwrap();
        state.reads += 1;
        if state.fail_reads > 0 {
            state.fail_reads -= 1;
            anyhow::bail!("transport failed")
        }
        let held = if state.posted_epoch == state.epoch { state.posted } else { None };
        Ok((state.epoch, held))
    }

    async fn post(
        &self,
        epoch: u64,
        head: Head,
        parent: Option<Head>,
        _signature: &str,
    ) -> anyhow::Result<PostOutcome> {
        let mut state = self.0.lock().unwrap();
        assert_eq!(epoch, state.epoch, "the signature's epoch must be the chain's current one");
        state.post_attempts += 1;
        if state.fail_posts > 0 {
            state.fail_posts -= 1;
            anyhow::bail!("the send's response never arrived: the transaction may still be broadcast")
        }
        if state.waiting {
            return Ok(PostOutcome::Waiting);
        }
        // The contract's chaining rule: a post must name the held head of its epoch as its parent
        // and beat its seq, or the chain rejects it — a missing or stale parent reverts NotParent.
        if state.posted_epoch == epoch
            && let Some(held) = state.posted
        {
            if parent != Some(held) {
                anyhow::bail!("NotParent: parent {parent:?} does not extend the held head {held:?}");
            }
            if head.seq <= held.seq {
                anyhow::bail!("NotNewer: seq {} does not beat the held seq {}", head.seq, held.seq);
            }
        }
        state.posts.push((epoch, parent, head));
        if !state.drop_after_accept && !state.hold {
            state.posted = Some(head);
            state.posted_epoch = epoch;
        }
        Ok(PostOutcome::Accepted)
    }
}
