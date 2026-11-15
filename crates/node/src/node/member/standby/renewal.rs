//! A serving operator's standby key (#595): a promotion consumes the member's standby key, and a
//! release move (an upgrade by `--predecessor`) voids it, while a candidate with no promotable key is
//! left out of the rotation set an election close adopts. While it serves, the operator keeps its
//! release's standby key, sealing a new one when it has none with authority, and registers it as a
//! following member does, asking the turn's members for a vouch. It runs no member link: the handover
//! endpoint is the operator's own.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::Address;
use anyhow::Context;
use engine_core::types::AccountId;
use futures_util::future::BoxFuture;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::config::{NodeConfig, TeeProvider};
use crate::custody::StandbyReader;
use crate::handover::evidence::Quoting;
use crate::liveness::chain::candidate_of;
use crate::liveness::link::Copy;
use crate::node::KeyStart;
use crate::node::member::standby::Standby;
use crate::node::member::standby::registration::remove_registration;
use crate::standby_key::chain::{StandbyChain, StandbyDomain};
use crate::standby_key::{StandbyKeys, StandbyStart};
use crate::tee::Attestor;
use crate::trusted_time::TrustedClock;

/// How often the replacement key's registration is checked, and asked for again once its vouch nears
/// expiry.
const EVERY: Duration = Duration::from_secs(60);

/// What the replacement key is kept with: what `Standby::open` reads from the config, owned.
pub(crate) struct Renewal {
    chain: StandbyReader,
    chain_id: u64,
    vault: Address,
    admin: AccountId,
    start: KeyStart,
    config_sha256: [u8; 32],
    dir: PathBuf,
    tee: (TeeProvider, Option<String>, String),
    attestor: Box<dyn Attestor>,
}

impl Renewal {
    /// The renewal `cfg` and `start` describe.
    /// Error cases:
    /// - `[log_heads]`, `[tee.sealing]` or the operator key is missing, or the custody quorum or the
    ///   attestor cannot be built
    pub(crate) fn new(cfg: &NodeConfig, start: &KeyStart) -> anyhow::Result<Self> {
        let log_heads = cfg.log_heads.as_ref().context("[log_heads]")?.contract;
        let sealing = cfg.tee.sealing.as_ref().context("an operator seals its keys")?;
        Ok(Self {
            chain: StandbyReader::connect_named(&cfg.custody, log_heads, "standby_renewal")?,
            chain_id: cfg.custody.chain_id,
            vault: Address::from(cfg.custody.vault.parse::<AccountId>()?.0),
            admin: cfg.operator()?.address,
            start: start.clone(),
            config_sha256: cfg.file_sha256,
            dir: sealing.dir.clone(),
            tee: (cfg.tee.provider, cfg.tee.processor.clone(), cfg.tee.kds_base.clone()),
            attestor: crate::node::keys::attestor_for(cfg.tee.provider, &crate::tee::clock::GuestClock::default())?,
        })
    }

    /// Seals the replacement key, then registers it until the schedule lists it as promotable, at
    /// trusted time's lower bound; runs for as long as the operator does.
    /// Error cases:
    /// - none: failed reads, seals and vouches are retried every `EVERY`
    pub(crate) async fn run(self, trusted: TrustedClock) -> anyhow::Result<()> {
        let (domain, candidate, keys) = loop {
            match self.open().await {
                Ok(opened) => break opened,
                Err(e) => warn!("opening a replacement standby key failed, trying again: {e:#}"),
            }
            tokio::time::sleep(EVERY).await;
        };
        info!(key = %keys.address(), %candidate, "the serving operator's standby key for its release");
        let Self { chain, dir, tee, attestor, .. } = self;
        let mut standby = Standby {
            keys: Arc::new(keys),
            candidate,
            domain,
            chain,
            copy: Copy::default(),
            tasks: JoinSet::new(),
            dir,
            written: None,
            unregistered: false,
            samples: watch::channel(None).0,
            answering: None,
            certificates: None,
        };
        let quoting = Quoting { provider: tee.0, processor: tee.1.as_deref(), kds_base: &tee.2 };
        loop {
            if let Some(reached) = trusted.reached_ms() {
                standby.register(&quoting, attestor.as_ref(), reached, None).await;
            }
            tokio::time::sleep(EVERY).await;
        }
    }

    /// The vault's registry and schedule, this operator's candidate, and its release's standby keys:
    /// the sealed ones while they hold authority, otherwise new ones (a promotion consumed the last, or
    /// a release move voided it), with any registration written for a replaced one removed.
    /// Error cases:
    /// - the chain cannot be read, the admin key belongs to no candidate, or the keys cannot be sealed
    async fn open(&self) -> anyhow::Result<(StandbyDomain, AccountId, StandbyKeys)> {
        let domain = StandbyDomain::read(&self.chain, self.chain_id, self.vault).await?;
        let read: BoxFuture<'_, anyhow::Result<Option<AccountId>>> = Box::pin(async {
            let block = self.chain.latest().await?;
            candidate_of(&block, &domain, self.admin).await
        });
        let admin = self.admin;
        let candidate = read.await?.with_context(|| format!("admin key {admin} belongs to no registered candidate"))?;
        let sealer = crate::node::keys::sealer(self.tee.0, &self.start)?;
        let opened =
            StandbyKeys::open_or_generate(&self.dir, sealer, self.config_sha256, &self.chain, &domain, candidate);
        match opened.await? {
            StandbyStart::Standby { keys, generated } => {
                if generated {
                    remove_registration(&self.dir)?;
                }
                Ok((domain, candidate, keys))
            }
            StandbyStart::Promoted { .. } => anyhow::bail!("the standby seal still holds the promoted key"),
        }
    }
}
