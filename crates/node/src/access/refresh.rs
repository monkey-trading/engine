//! The supervised loop that refreshes the API's screening decisions before they run out, for each
//! account and client address that sent a request within the freshness bound, so an active trader's
//! requests never wait on the source. Each refresh takes a screening slot outside the reserve, but
//! never the last of them, within its account's and client's shares; one that finds none, or whose
//! source call fails, tries again on the next turn while the decision lasts.

use std::net::IpAddr;
use std::sync::Arc;

use alloy::primitives::keccak256;
use anyhow::Context;
use engine_core::access::AccessEvidence;
use engine_core::eip712::{B256, Eip712Domain};
use engine_core::types::AccountId;
use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{Instrument, debug, info_span};

use crate::access::decisions::Pair;
use crate::access::{AccessClient, Claim, ScreeningRequest, SlotKind};
use crate::clock::Clock;

/// Tags a refresh's slot key, so it never matches a signed request's digest.
const REFRESH_TAG: &[u8] = b"engine.screening-refresh";

impl AccessClient {
    /// Refreshes, each turn, the active pairs whose decisions are due, at most `refresh_limit` at once,
    /// checking each decision against `domain`, at `clock`'s time. Runs until a refresh panics.
    /// Error cases:
    /// - a refresh task panics
    pub(crate) async fn refresh(self: Arc<Self>, domain: Eip712Domain, clock: Arc<dyn Clock>) -> anyhow::Result<()> {
        let mut turns = interval(self.api_decisions.tick());
        turns.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut calls = JoinSet::new();
        loop {
            tokio::select! {
                _ = turns.tick() => {
                    let limit = self.api_decisions.refresh_limit.saturating_sub(calls.len());
                    for pair in self.api_decisions.due(clock.now_ms(), limit) {
                        let span = info_span!("screening_refresh", id = %pair.0);
                        let call = Self::refresh_pair(self.clone(), pair, domain.clone(), clock.clone());
                        calls.spawn(call.instrument(span));
                    }
                }
                Some(joined) = calls.join_next() => joined.context("a screening refresh panicked")?,
            }
        }
    }

    /// Asks the source again for `pair`'s decision while it holds a slot, keeps the answer if it
    /// verifies, and ends the pair's refresh.
    async fn refresh_pair(self: Arc<Self>, pair: Pair, domain: Eip712Domain, clock: Arc<dyn Clock>) {
        let (account, peer) = pair;
        let claim = Claim { account, client: Some(peer), request: refresh_key(account, peer), kind: SlotKind::Refresh };
        match self.api_slots.take(claim) {
            Ok(_slot) => {
                let evidence = self.screen(&ScreeningRequest::client(account, peer)).await;
                let kept = match &evidence {
                    AccessEvidence::Screened(screening) => {
                        self.api_decisions.store(account, peer, screening, &domain, clock.now_ms(), false)
                    }
                    AccessEvidence::Failed { .. } => false,
                };
                debug!(kept, "screening decision refreshed");
            }
            Err(refusal) => debug!(?refusal, "screening refresh found no slot"),
        }
        self.api_decisions.refreshed(pair, clock.now_ms());
    }
}

/// The slot key of `account`'s refresh for `peer`.
fn refresh_key(account: AccountId, peer: IpAddr) -> B256 {
    let peer = match peer {
        IpAddr::V4(address) => [&[4][..], &address.octets()].concat(),
        IpAddr::V6(address) => [&[6][..], &address.octets()].concat(),
    };
    keccak256([REFRESH_TAG, &account.0[..], &peer[..]].concat())
}
