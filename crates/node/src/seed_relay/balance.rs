//! The relayer's balance: exported, warned on below `min_balance`, and warned on when unreadable.

use std::time::Duration;

use alloy::primitives::utils::format_ether;
use tokio::time::Instant;
use tracing::{info, warn};

use super::{SeedChains, SeedRelayer};

/// How long the balance may stay unreadable before the relayer warns: `seed_relayer_unfunded` and the
/// balance gauge cannot move meanwhile, though relays keep spending the key.
pub(super) const UNREADABLE_AFTER: Duration = Duration::from_secs(600);

impl<C: SeedChains> SeedRelayer<C> {
    /// Exports the relayer's balance and warns while it is below `min_balance`. A read that fails
    /// changes nothing until it has failed for `UNREADABLE_AFTER`, when `seed_relayer_balance_unreadable`
    /// warns, naming the cause, until a read succeeds: no relay read uses the same call at the
    /// providers' finalized block, so nothing else alerts.
    pub(super) async fn check_balance(&mut self) {
        let balance = match self.chains.balance().await {
            Ok(balance) => balance,
            Err(error) => {
                let since = *self.unreadable_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= UNREADABLE_AFTER && !self.unreadable {
                    self.unreadable = true;
                    warn!(
                        alert = "seed_relayer_balance_unreadable",
                        relayer = %self.relayer,
                        "the seed relayer's balance has been unreadable for {:?}, so seed_relayer_unfunded cannot \
                         fire: {error:#}",
                        since.elapsed()
                    );
                }
                return;
            }
        };
        self.unreadable_since = None;
        if std::mem::take(&mut self.unreadable) {
            info!(alert_resolved = "seed_relayer_balance_unreadable", relayer = %self.relayer, "the balance reads again");
        }
        let ether = format_ether(balance);
        self.metrics.balance_eth.set(ether.parse().unwrap_or(f64::NAN));
        if balance < self.min_balance {
            self.unfunded = true;
            warn!(
                alert = "seed_relayer_unfunded",
                relayer = %self.relayer,
                "the seed relayer holds {ether} ETH, below {}: relays stop when it cannot pay for one",
                format_ether(self.min_balance)
            );
        } else if std::mem::take(&mut self.unfunded) {
            info!(alert_resolved = "seed_relayer_unfunded", relayer = %self.relayer, "the seed relayer holds {ether} ETH");
        }
    }
}
