//! A member's standby side of the liveness rule (#595), beside its following: its standby keys,
//! sealed for its release (`standby_key`); the member link it answers on its registered handover
//! endpoint; its liveness caller, with `[liveness]`; the vouched registration of its standby key; and
//! the check, each stint, whether a promotion made that key the vault's signer.
//!
//! The schedule takes a registration only from the candidate's own account, which the node does not
//! hold: once a vouch arrives, the node writes the registration, with its `registerStandby` call, to
//! `standby-registration.json` in the sealing directory and raises `standby_unregistered` naming it,
//! for the candidate to send, until the schedule lists the key as promotable.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, Bytes};
use alloy::sol_types::SolCall;
use anyhow::Context;
use engine_core::types::{AccountId, Millis};
use futures_util::future::BoxFuture;
use serde::Serialize;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::config::NodeConfig;
use crate::custody::StandbyReader;
use crate::handover::evidence::Quoting;
use crate::liveness::caller::{Caller, Endpoint};
use crate::liveness::chain::{
    LivenessDomain, Member as SetMember, candidate_of, members, promotable, registerStandbyCall,
};
use crate::liveness::link::{Asking, Copy, Reply, Request, Server, Tcp, describe};
use crate::node::KeyStart;
use crate::node::member::turn::Opened;
use crate::node::member::{Member, PostedHead, Sampling};
use crate::node::ready::Start;
use crate::standby::Followed;
use crate::standby_key::chain::{Promotion, StandbyChain, StandbyDomain, StandbyState, standby_state};
use crate::standby_key::vouch::{StandbyRegistration, VouchReply, VouchRequest, request};
use crate::standby_key::{StandbyKeys, StandbyStart};
use crate::tee::Attestor;
use crate::trusted_time::{Sample, TrustedClock};

/// The registration file the candidate sends from.
const REGISTRATION: &str = "standby-registration.json";
/// How long before a written registration's vouch expires a new one is asked for.
const RENEW_BEFORE: Duration = Duration::from_secs(10 * 60);
/// How long a vouch request to the operator's API may take.
const VOUCH_TIMEOUT: Duration = Duration::from_secs(60);

/// A member's standby keys and what runs beside its following on them.
pub(crate) struct Standby {
    keys: Arc<StandbyKeys>,
    candidate: AccountId,
    domain: StandbyDomain,
    chain: StandbyReader,
    copy: Copy,
    tasks: JoinSet<()>,
    /// The sealing directory, where the registration is written.
    dir: PathBuf,
    /// The written registration's vouch expiry, in Unix seconds.
    written: Option<u64>,
    /// Whether the `standby_unregistered` episode is open.
    unregistered: bool,
    /// The member's trusted time samples, which the member link's vouches check expiries against.
    samples: watch::Sender<Option<Sample>>,
}

/// The registration as the file carries it.
#[derive(Serialize)]
struct Written<'a> {
    candidate: AccountId,
    schedule: Address,
    key: AccountId,
    nonce: u64,
    expiry: u64,
    voucher: AccountId,
    vouch: &'a engine_core::eip712::Signature,
    /// `registerStandby(key, nonce, expiry, vouch)`, to send from the candidate's account.
    calldata: Bytes,
}

/// Whether a member runs the rule's standby side: rotation on, `[log_heads]`, EVM custody, sealing.
pub(crate) fn applies(cfg: &NodeConfig) -> bool {
    cfg.operator.rotation_enabled
        && cfg.log_heads.is_some()
        && cfg.custody.provider == crate::config::CustodyProvider::Evm
        && cfg.tee.sealing.is_some()
}

impl Standby {
    /// Opens or generates this release's standby keys for the candidate whose admin key `cfg` names,
    /// and starts the member link and, with `[liveness]`, the liveness caller beside the following.
    /// With them it returns the promotion that made the key the vault's signer, if its reseal waits.
    /// Error cases:
    /// - the chain cannot be read, the admin key belongs to no candidate, or the keys cannot be opened
    ///   or sealed
    pub(crate) async fn open(cfg: &NodeConfig, start: &KeyStart) -> anyhow::Result<(Self, Option<Promotion>)> {
        let log_heads = cfg.log_heads.as_ref().context("[log_heads]")?.contract;
        let sealing = cfg.tee.sealing.as_ref().context("a member seals its keys")?;
        let chain = StandbyReader::connect(&cfg.custody, log_heads)?;
        let vault = Address::from(cfg.custody.vault.parse::<AccountId>()?.0);
        let domain = StandbyDomain::read(&chain, cfg.custody.chain_id, vault).await?;
        let admin = cfg.operator()?.address;
        let read: BoxFuture<'_, anyhow::Result<Option<AccountId>>> = Box::pin(async {
            let block = chain.latest().await?;
            candidate_of(&block, &domain, admin).await
        });
        let candidate = read.await?.with_context(|| format!("admin key {admin} belongs to no registered candidate"))?;
        let sealer = crate::node::keys::sealer(cfg.tee.provider, start)?;
        let opened = StandbyKeys::open_or_generate(&sealing.dir, sealer, cfg.file_sha256, &chain, &domain, candidate);
        let (keys, promotion) = match opened.await? {
            StandbyStart::Standby { keys, generated } => {
                info!(key = %keys.address(), %candidate, generated, "the member's standby key is open");
                (keys, None)
            }
            StandbyStart::Promoted { keys, promotion } => (keys, Some(promotion)),
        };
        let mut standby = Self {
            keys: Arc::new(keys),
            candidate,
            domain,
            chain,
            copy: Copy::default(),
            tasks: JoinSet::new(),
            dir: sealing.dir.clone(),
            written: None,
            unregistered: false,
            samples: watch::channel(None).0,
        };
        let samples = standby.samples.subscribe();
        standby.spawn(cfg, samples)?;
        Ok((standby, promotion))
    }

    /// Starts the member link on the handover endpoint and, with `[liveness]`, the liveness caller.
    /// Error cases:
    /// - a custody quorum or the caller's endpoint cannot be built
    fn spawn(&mut self, cfg: &NodeConfig, samples: watch::Receiver<Option<Sample>>) -> anyhow::Result<()> {
        let log_heads = cfg.log_heads.as_ref().context("[log_heads]")?;
        let vault = self.domain.vault;
        let handover = cfg.tee.sealing.as_ref().and_then(|sealing| sealing.handover.as_ref());
        if let Some(handover) = handover {
            let clock: Arc<dyn crate::clock::Clock> = Arc::new(crate::clock::SystemClock);
            let platform = self.keys.platform();
            let server = Server {
                listen: handover.listen,
                deployment: (cfg.custody.chain_id, vault),
                member: self.candidate,
                copy: self.copy.clone(),
                standby: self.keys.clone(),
                provider: cfg.tee.provider,
                platform,
                trusted: TrustedClock::new(clock, samples, cfg.time.max_drift),
                chain: StandbyReader::connect_named(&cfg.custody, log_heads.contract, "member_link")?,
                domain: self.domain,
            };
            self.tasks.spawn(async move {
                let _ = server.run().await;
            });
        }
        if let (Some(liveness), Some(checkpoints)) = (&cfg.liveness, &cfg.checkpoints) {
            let caller = Caller {
                chain: StandbyReader::connect_named(&cfg.custody, log_heads.contract, "liveness_caller")?,
                sending: Endpoint::new(checkpoints, liveness, cfg.custody.chain_id)?,
                domain: LivenessDomain { standby: self.domain, log_heads: log_heads.contract },
                copy: self.copy.clone(),
                min_balance: liveness.min_balance,
            };
            self.tasks.spawn(async move {
                if let Err(e) = caller.run().await {
                    warn!("the liveness caller stopped: {e:#}");
                }
            });
        }
        Ok(())
    }

    /// Takes the member's latest trusted time sample, for the vouches the member link answers.
    pub(crate) fn sampled(&self, sample: Sample) {
        self.samples.send_replace(Some(sample));
    }

    /// Stops the link and the caller, freeing the handover endpoint.
    pub(crate) async fn stop(mut self) {
        self.tasks.shutdown().await;
    }

    /// What the member link answers from, for the follower to record into.
    pub(crate) fn copy(&self) -> Copy {
        self.copy.clone()
    }

    /// The candidate the keys are for.
    pub(crate) fn candidate(&self) -> AccountId {
        self.candidate
    }

    /// The promotion that made the standby key the vault's signer, while it is its turn's latest
    /// assignment, at the latest agreed block.
    /// Error cases:
    /// - the chain cannot be read
    pub(crate) async fn promotion(&self) -> anyhow::Result<Option<Promotion>> {
        let state: BoxFuture<'_, anyhow::Result<StandbyState>> =
            Box::pin(standby_state(&self.chain, &self.domain, self.candidate, self.keys.address()));
        Ok(match state.await? {
            StandbyState::Promoted(promotion) => Some(promotion),
            StandbyState::Promotable | StandbyState::Unusable(_) => None,
        })
    }

    /// The members of the set the turn in effect was drawn from, at the latest agreed block; none
    /// before a turn is drawn.
    /// Error cases:
    /// - the chain cannot be read
    pub(crate) async fn turn_members(&self) -> anyhow::Result<Vec<SetMember>> {
        let log_heads = Address::ZERO;
        let read: BoxFuture<'_, anyhow::Result<Vec<SetMember>>> = Box::pin(async move {
            let block = self.chain.latest().await?;
            let domain = LivenessDomain { standby: self.domain, log_heads };
            let set = crate::liveness::chain::turn_set(&block, &domain).await?;
            match set {
                Some(set) => members(&block, &self.domain, set).await,
                None => Ok(Vec::new()),
            }
        });
        read.await
    }

    /// Asks for a vouch and writes the registration, while the schedule does not list the key as
    /// promotable and no written registration's vouch is still good: from `operator`'s API first,
    /// then from each member of the turn in effect's set on its handover endpoint. Quotes with
    /// `attestor` at `reached`, trusted time's lower bound.
    /// Error cases:
    /// - none: a failed read or vouch is retried at the next call
    pub(crate) async fn register(
        &mut self,
        cfg: &NodeConfig,
        attestor: &dyn Attestor,
        reached: Millis,
        operator: Option<&Followed>,
    ) {
        let read: BoxFuture<'_, anyhow::Result<Option<AccountId>>> = Box::pin(async {
            let block = self.chain.latest().await?;
            promotable(&block, &self.domain, self.candidate).await
        });
        match read.await {
            Ok(Some(key)) if key == self.keys.address() => {
                if std::mem::take(&mut self.unregistered) {
                    info!(alert_resolved = "standby_unregistered", key = %key, "the schedule lists the standby key as promotable");
                }
                return;
            }
            Ok(_) => {}
            Err(e) => return warn!("reading whether the standby key is registered failed: {e:#}"),
        }
        let now = u64::try_from(reached / 1000).unwrap_or(0);
        if self.written.is_some_and(|expiry| expiry > now + RENEW_BEFORE.as_secs()) {
            return;
        }
        let quoting = Quoting::from(&cfg.tee);
        let asked = request(&quoting, attestor, &self.keys, &self.domain, self.candidate, reached).await;
        let request = match asked {
            Ok(request) => request,
            Err(e) => return warn!("quoting the standby key for a vouch failed: {e:#}"),
        };
        let Some(reply) = self.vouch(&request, operator).await else {
            return warn!(key = %self.keys.address(), "no operator or member vouched for the standby key: asking again later");
        };
        match StandbyRegistration::new(&self.domain, &request, &reply)
            .and_then(|registration| self.write(&registration))
        {
            Ok(calldata) => {
                self.written = Some(request.expiry);
                if !std::mem::replace(&mut self.unregistered, true) {
                    warn!(
                        alert = "standby_unregistered",
                        key = %self.keys.address(),
                        candidate = %self.candidate,
                        "the standby key is not registered: send registerStandby to the schedule {} from the \
                         candidate's account before the vouch expires at {} (calldata {calldata}, also in {REGISTRATION})",
                        self.domain.schedule,
                        request.expiry
                    );
                }
            }
            Err(e) => warn!("the vouch for the standby key is unusable: {e:#}"),
        }
    }

    /// The first vouch for `request`: `operator`'s API, then each member of the turn's set but this one.
    async fn vouch(&self, request: &VouchRequest, operator: Option<&Followed>) -> Option<VouchReply> {
        if let Some(operator) = operator {
            match vouch_from_api(operator, request).await {
                Ok(reply) => return Some(reply),
                Err(e) => warn!(operator = %operator.origin, "the operator did not vouch for the standby key: {e:#}"),
            }
        }
        let members = self.turn_members().await.unwrap_or_default();
        let ask = Request::Vouch { request: Box::new(request.clone()) };
        for member in members.iter().filter(|member| member.candidate != self.candidate && !member.endpoint.is_empty())
        {
            match Tcp.ask(&member.endpoint, &ask).await {
                Ok(Reply::Vouch { reply }) => return Some(reply),
                Ok(other) => warn!(member = %member.candidate, "the member did not vouch: {}", describe(&other)),
                Err(e) => warn!(member = %member.candidate, "asking a member to vouch failed: {e:#}"),
            }
        }
        None
    }

    /// Writes `registration` and its call to the registration file, returning the call's hex.
    /// Error cases:
    /// - the file cannot be written
    fn write(&self, registration: &StandbyRegistration) -> anyhow::Result<String> {
        let calldata: Bytes = registerStandbyCall {
            key: Address::from(registration.key.0),
            nonce: registration.nonce,
            expiry: registration.expiry,
            vouch: Bytes::from(registration.vouch.to_vec()),
        }
        .abi_encode()
        .into();
        let written = Written {
            candidate: self.candidate,
            schedule: self.domain.schedule,
            key: registration.key,
            nonce: registration.nonce,
            expiry: registration.expiry,
            voucher: registration.voucher,
            vouch: &registration.vouch,
            calldata: calldata.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&written).context("encoding the registration")?;
        crate::tee::sealing::write_replacing(&self.dir.join(REGISTRATION), &bytes)?;
        Ok(calldata.to_string())
    }

    /// Stops the link and the caller, freeing the handover endpoint, and gives back the standby keys.
    /// Error cases:
    /// - a task still holds the keys
    pub(crate) async fn into_keys(mut self) -> anyhow::Result<StandbyKeys> {
        self.tasks.shutdown().await;
        Arc::try_unwrap(self.keys).map_err(|_| anyhow::anyhow!("the standby keys are still in use"))
    }
}

/// `operator`'s vouch for `request` over its API.
/// Error cases:
/// - the request fails, times out, or is refused
async fn vouch_from_api(operator: &Followed, request: &VouchRequest) -> anyhow::Result<VouchReply> {
    let client = crate::standby::archive::archive_client(operator.danger_accept_invalid_certs)?;
    let url = format!("{}/v1/standby/vouch", operator.origin.trim_end_matches('/'));
    let response = tokio::time::timeout(VOUCH_TIMEOUT, client.post(&url).json(request).send())
        .await
        .context("the vouch request timed out")??;
    let status = response.status();
    let body = response.bytes().await.context("reading the vouch")?;
    anyhow::ensure!(status.is_success(), "{status}: {}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).context("the vouch does not parse")
}

impl<C: crate::checkpoints::CheckpointChain, H: PostedHead, T: Sampling> Member<'_, C, H, T> {
    /// Keeps the member's standby side beside its following: opens it once the rule applies, links the
    /// follower's copy to it, starts the member as the operator once a promotion makes its standby key
    /// the vault's signer, and otherwise registers the key while the schedule does not list it as
    /// promotable, asking `operator` first.
    /// Error cases:
    /// - the promoted start fails (see `start_promoted`), which consumes the standby side, or the copy
    ///   does not open for it
    pub(super) async fn keep_standby(
        &mut self,
        standby: &mut Option<Standby>,
        follower: &mut Option<Opened>,
        operator: Option<&Followed>,
    ) -> anyhow::Result<Option<Start>> {
        if !applies(self.cfg) {
            return Ok(None);
        }
        let mut pending = None;
        if standby.is_none() {
            match Standby::open(self.cfg, &self.start).await {
                Ok((opened, promotion)) => (*standby, pending) = (Some(opened), promotion),
                Err(e) => {
                    warn!("opening the member's standby keys failed, trying again: {e:#}");
                    return Ok(None);
                }
            }
        }
        let Some(open) = standby.as_mut() else { return Ok(None) };
        if let Some((replica, ..)) = follower.as_mut()
            && !replica.linked()
        {
            replica.link_with(open.copy());
        }
        let promotion = match pending {
            Some(promotion) => Some(promotion),
            None => open.promotion().await.unwrap_or_else(|e| {
                warn!("reading whether a promotion named this member failed: {e:#}");
                None
            }),
        };
        if let Some(promotion) = promotion {
            info!(
                turn = promotion.turn,
                position = promotion.position,
                "a promotion made this member's standby key the vault's signer"
            );
            let opened = match follower.take() {
                Some(opened) => opened,
                None => self.open(None).await?,
            };
            let standby = standby.take().expect("open above");
            return self.start_promoted(standby, promotion, opened).await.map(Some);
        }
        if let Ok(reached) = self.reached().await {
            if let Some(sample) = self.sample {
                open.sampled(sample);
            }
            open.register(self.cfg, self.attestor.as_ref(), reached, operator).await;
        }
        Ok(None)
    }
}
