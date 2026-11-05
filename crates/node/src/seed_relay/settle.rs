//! Which transaction, if any, the relayer's next send takes the place of, and at which nonce.

use alloy::primitives::B256;
use tracing::warn;

use super::{InFlight, SeedChains, SeedRelayer};

/// The nonce a relay goes out at, and the transaction it outbids when that one still waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Slot {
    pub nonce: u64,
    pub replacing: Option<B256>,
}

impl<C: SeedChains> SeedRelayer<C> {
    /// Settles the relay in flight. `None` holds new relays back; `Some(slot)` lets one go out at the
    /// slot's nonce: the relayer's `latest` nonce when nothing of this node's waits, and the in-flight
    /// relay's own when it has waited `resend_after` for a block, so a replacement never queues behind
    /// the transaction it replaces, whether or not the answer to an earlier replacement was lost. Once
    /// the relayer's `latest` nonce passes the in-flight relay's without its receipt, another
    /// transaction took the nonce and the node stops tracking the relay.
    /// Error cases:
    /// - the relay in flight reverted, or the receipt or the nonces cannot be read
    pub(super) async fn settle(&mut self) -> anyhow::Result<Option<Slot>> {
        if let Some(InFlight { hash, turn, nonce, at }) = self.in_flight {
            match self.chains.receipt(hash).await? {
                Some(true) => {
                    self.mined.insert(turn, tokio::time::Instant::now());
                    self.in_flight = None;
                }
                Some(false) => {
                    self.in_flight = None;
                    anyhow::bail!("turn {turn}'s relay {hash} reverted");
                }
                None if at.elapsed() < self.resend_after => return Ok(None),
                None => {
                    let (latest, _) = self.chains.nonces().await?;
                    if latest <= nonce {
                        self.fail(format!(
                            "relay {hash} for turn {turn} has waited {:?} for a block: replacing it at its nonce {nonce}",
                            at.elapsed()
                        ));
                        return Ok(Some(Slot { nonce, replacing: Some(hash) }));
                    }
                    warn!(
                        "the relayer's nonce {nonce} was used by a transaction other than relay {hash} for turn \
                         {turn}: no longer tracking it"
                    );
                    self.in_flight = None;
                }
            }
        }
        // With nothing of this node's in flight, a pending transaction at the relayer's nonce is one it
        // does not track, sent before a restart or by a send whose answer was lost; a relay sent now
        // would queue behind it.
        let (latest, pending) = self.chains.nonces().await?;
        if pending > latest {
            let since = *self.held_since.get_or_insert_with(tokio::time::Instant::now);
            if since.elapsed() >= self.resend_after {
                self.fail(format!(
                    "a transaction this node is not tracking has waited {:?} at the relayer's nonce {latest}: relays \
                     wait for it, and it can be replaced by hand",
                    since.elapsed()
                ));
            }
            return Ok(None);
        }
        self.held_since = None;
        Ok(Some(Slot { nonce: latest, replacing: None }))
    }
}
