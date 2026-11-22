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

use alloy::primitives::Address;
use anyhow::Context;
use engine_core::types::{AccountId, Millis};
use futures_util::future::BoxFuture;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::config::NodeConfig;
use crate::custody::StandbyReader;
use crate::handover::evidence::Quoting;
use crate::liveness::caller::{Caller, Endpoint, Reached};
use crate::liveness::chain::{LivenessDomain, Member as SetMember, candidate_of, members, promotable};
use crate::liveness::link::{Asking, Copy, Reply, Request, Server, Tcp, describe};
use crate::log_check::answer::Answering;
use crate::metrics::Metrics;
use crate::node::KeyStart;
use crate::node::member::standby::registration::{REGISTRATION, remove_registration};
use crate::standby::Followed;
use crate::standby_key::chain::{Promotion, StandbyChain, StandbyDomain, StandbyState, may_vouch, standby_state};
use crate::standby_key::vouch::{Service, StandbyRegistration, VouchReply, VouchRequest, request};
use crate::standby_key::{StandbyKeys, StandbyStart};
use crate::tee::Attestor;
use crate::tee::certificates::CertificateStore;
use crate::tls::TlsIdentity;
use crate::tls::acme::CertificateTask;
use crate::trusted_time::{Sample, TrustedClock};

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
    /// The member link and the liveness caller, each returning its name if it ever ends.
    tasks: JoinSet<&'static str>,
    /// The sealing directory, where the registration is written.
    dir: PathBuf,
    /// The written registration's vouch expiry, in Unix seconds, and its voucher.
    written: Option<(u64, AccountId)>,
    /// Whether the `standby_unregistered` episode is open.
    unregistered: bool,
    /// The member's trusted time samples, which the member link's vouches check expiries against.
    samples: watch::Sender<Option<Sample>>,
    /// The member's answers to the operator's log checks, which the member link gives on the endpoint.
    answering: Option<Arc<Answering>>,
    /// The standby key's certificate store, with `[api.tls.acme]` (`certificate`), and the node's
    /// metrics, which its certificate's gauges report in.
    certificates: Option<(Arc<CertificateStore>, Arc<Metrics>)>,
    /// A serving operator's own vouching with the vault's signer key (`renewal`): a set of no other
    /// member has no one else to ask.
    vouching: Option<Arc<Service<StandbyReader>>>,
    /// Held for each send from the liveness caller's account, shared with the process's fallback fixer.
    sends: Arc<crate::liveness::fallback::Sends>,
}

/// Whether a member runs the rule's standby side: rotation on, `[log_heads]`, EVM custody, sealing.
pub(crate) fn applies(cfg: &NodeConfig) -> bool {
    cfg.operator.rotation_enabled
        && cfg.log_heads.is_some()
        && cfg.custody.provider == crate::config::CustodyProvider::Evm
        && cfg.tee.sealing.is_some()
}

/// Whether the rule's standby side applies and `cfg`'s admin key belongs to a registered candidate at
/// the latest agreed block: only a candidate has a standby key to prepare before rotation turns on, and
/// any other node's rotation check decides at once. `false` when the chain cannot be read, which the
/// rotation check then reads for itself.
pub(crate) async fn prepares(cfg: &NodeConfig) -> bool {
    if !applies(cfg) {
        return false;
    }
    let read = async {
        let log_heads = cfg.log_heads.as_ref().context("[log_heads]")?.contract;
        let chain = StandbyReader::connect_named(&cfg.custody, log_heads, "candidate")?;
        let vault = Address::from(cfg.custody.vault.parse::<AccountId>()?.0);
        let domain = StandbyDomain::read(&chain, cfg.custody.chain_id, vault).await?;
        let block = chain.latest().await?;
        candidate_of(&block, &domain, cfg.operator()?.address).await
    };
    let read: BoxFuture<'_, anyhow::Result<Option<AccountId>>> = Box::pin(read);
    read.await
        .inspect_err(|e| warn!("reading whether this node's admin key is a candidate's failed: {e:#}"))
        .is_ok_and(|candidate| candidate.is_some())
}

/// Every key the vault names at the latest agreed block from signer epoch `from` on, while the rule
/// applies: a promotion names its key there before finality does (#595), and a later one may name
/// another before the first is final, so a rebuilt copy replays under each, as the live one did.
/// None while the rule does not apply, since no promotion names a key then.
/// Error cases:
/// - the custody quorum cannot be built or read
pub(crate) async fn named_since(cfg: &NodeConfig, from: u64) -> anyhow::Result<Vec<AccountId>> {
    if !applies(cfg) {
        return Ok(Vec::new());
    }
    named_at_latest(cfg, from).await
}

/// Every key the vault names at the latest agreed block from signer epoch `from` on, up to
/// `UNFINAL_EPOCHS` epochs past it (`signers_from`), read through the custody quorum `[log_heads]` sits
/// beside.
/// Error cases:
/// - there is no `[log_heads]`, or the custody quorum cannot be built or read
pub(crate) async fn named_at_latest(cfg: &NodeConfig, from: u64) -> anyhow::Result<Vec<AccountId>> {
    let log_heads = cfg.log_heads.as_ref().context("[log_heads]")?.contract;
    let chain = StandbyReader::connect_named(&cfg.custody, log_heads, "signers")?;
    let vault = Address::from(cfg.custody.vault.parse::<AccountId>()?.0);
    let read: BoxFuture<'_, anyhow::Result<Vec<AccountId>>> = Box::pin(async {
        let block = chain.latest().await?;
        crate::standby_key::chain::signers_from(&block, vault, from).await
    });
    read.await
}

impl Standby {
    /// Opens or generates this release's standby keys for the candidate whose admin key `cfg` names,
    /// and starts the member link, with `[liveness]` the liveness caller, and with `[api.tls.acme]` the
    /// standby certificate beside the following.
    /// With them it returns the promotion that made the key the vault's signer, if its reseal waits.
    /// Error cases:
    /// - the chain cannot be read, the admin key belongs to no candidate, or the keys cannot be opened
    ///   or sealed
    pub(crate) async fn open(
        cfg: &NodeConfig,
        start: &KeyStart,
        answering: Option<Arc<Answering>>,
        metrics: Arc<Metrics>,
        sends: Arc<crate::liveness::fallback::Sends>,
    ) -> anyhow::Result<(Self, Option<Promotion>)> {
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
                // A registration written for the key this one replaced is no longer to be sent.
                if generated {
                    remove_registration(&sealing.dir)?;
                }
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
            answering,
            certificates: certificate::store(cfg, start).map(|store| (store, metrics)),
            vouching: None,
            sends,
        };
        let samples = standby.samples.subscribe();
        standby.spawn(cfg, samples)?;
        Ok((standby, promotion))
    }

    /// Starts the member link on the handover endpoint, with `[liveness]` the liveness caller, and with
    /// the standby certificate store its certificate on the API's listener.
    /// Error cases:
    /// - a custody quorum or the caller's endpoint cannot be built, or the standby TLS key does not load
    fn spawn(&mut self, cfg: &NodeConfig, samples: watch::Receiver<Option<Sample>>) -> anyhow::Result<()> {
        if let Some((certificates, metrics)) = &self.certificates {
            let identity = TlsIdentity::new(self.keys.tls_key()?)?;
            let trusted = TrustedClock::new(Arc::new(crate::clock::SystemClock), samples.clone(), cfg.time.max_drift);
            let sealed = certificate::sealed(&self.keys, certificates, trusted);
            if let Some(task) = CertificateTask::configured(&cfg.api, identity.key, metrics, Some(sealed))? {
                let (listen, api) = (cfg.node.listen, cfg.api.clone());
                self.tasks.spawn(async move {
                    certificate::run(listen, identity.acceptors, api, task).await;
                    "standby certificate"
                });
            }
        }
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
                cpu_models: cfg.tee.cpu_models.clone(),
                platform,
                trusted: TrustedClock::new(clock, samples.clone(), cfg.time.max_drift),
                chain: StandbyReader::connect_named(&cfg.custody, log_heads.contract, "member_link")?,
                domain: self.domain,
                answering: self.answering.clone(),
            };
            self.tasks.spawn(async move { match server.run().await {} });
        }
        if let (Some(liveness), Some(checkpoints)) = (&cfg.liveness, &cfg.checkpoints) {
            let caller = Caller {
                chain: StandbyReader::connect_named(&cfg.custody, log_heads.contract, "liveness_caller")?,
                sending: Endpoint::new(checkpoints, liveness, cfg.custody.chain_id)?,
                domain: LivenessDomain { standby: self.domain, log_heads: log_heads.contract },
                copy: self.copy.clone(),
                min_balance: liveness.min_balance,
                poll: liveness.read_interval,
                clock: Arc::new(Reached(TrustedClock::new(
                    Arc::new(crate::clock::SystemClock),
                    samples,
                    cfg.time.max_drift,
                ))),
                sends: self.sends.clone(),
            };
            self.tasks.spawn(async move {
                if let Err(e) = caller.run().await {
                    warn!("the liveness caller stopped: {e:#}");
                }
                "liveness caller"
            });
        } else {
            warn!("no [liveness] caller: this member witnesses, promotes and relays nothing for a failed operator");
        }
        Ok(())
    }

    /// Restarts the member link and the liveness caller once either has ended or panicked, with
    /// `liveness_task_restarted`: the member never follows on without them.
    /// Error cases:
    /// - a custody quorum or the caller's endpoint cannot be built again
    pub(crate) async fn supervise(&mut self, cfg: &NodeConfig) -> anyhow::Result<()> {
        let mut ended = Vec::new();
        while let Some(joined) = self.tasks.try_join_next() {
            ended.push(joined.map_or_else(|e| format!("a task that panicked ({e})"), |name| format!("the {name}")));
        }
        if ended.is_empty() {
            // A restart that failed left nothing running: try it again.
            return match self.tasks.is_empty() {
                true => self.spawn(cfg, self.samples.subscribe()),
                false => Ok(()),
            };
        }
        warn!(
            alert = "liveness_task_restarted",
            "{} of this member stopped: restarting the member link and the liveness caller",
            ended.join(" and ")
        );
        self.tasks.shutdown().await;
        let samples = self.samples.subscribe();
        self.spawn(cfg, samples)
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

    /// The standby key's address.
    pub(crate) fn address(&self) -> AccountId {
        self.keys.address()
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
            StandbyState::Promotable | StandbyState::Unregistered | StandbyState::Unusable(_) => None,
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
                Some(turn) => members(&block, &self.domain, turn).await,
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
        quoting: &Quoting<'_>,
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
        if let Some((expiry, voucher)) = self.written.filter(|(expiry, _)| *expiry > now + RENEW_BEFORE.as_secs()) {
            // Good until its renewal, while the schedule still takes its voucher.
            match self.takes(voucher).await {
                Ok(true) => return,
                Ok(false) => {
                    info!(%voucher, expiry, "the written registration's voucher may no longer vouch: asking again")
                }
                Err(e) => return warn!("reading whether the written vouch's voucher may vouch failed: {e:#}"),
            }
        }
        let asked = request(quoting, attestor, &self.keys, &self.domain, self.candidate, reached).await;
        let request = match asked {
            Ok(request) => request,
            Err(e) => return warn!("quoting the standby key for a vouch failed: {e:#}"),
        };
        // The operator takes this member's statuses on it until the registration lands.
        self.copy.attested(&request);
        let Some(registration) = self.vouched(&request, operator).await else {
            return warn!(key = %self.keys.address(), "no operator or member vouched for the standby key: asking again later");
        };
        match self.write(&registration) {
            Ok(calldata) => {
                self.written = Some((request.expiry, registration.voucher));
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
            Err(e) => warn!("writing the standby key's registration failed: {e:#}"),
        }
    }

    /// The first usable registration for `request`: a serving operator's own vouch, a vouch from
    /// `operator`'s API, then from each member of the turn's set but this one, passing over any that
    /// does not recover to its voucher or whose voucher the schedule does not take (`usable`).
    async fn vouched(&self, request: &VouchRequest, operator: Option<&Followed>) -> Option<StandbyRegistration> {
        if let Some(own) = &self.vouching {
            match own.vouch(request).await.map(|reply| self.usable(request, reply)) {
                Ok(usable) => match usable.await {
                    Ok(registration) => return Some(registration),
                    Err(e) => warn!("this operator's own vouch is unusable: {e:#}"),
                },
                Err(e) => warn!("this operator did not vouch for its standby key: {e:#}"),
            }
        }
        if let Some(operator) = operator {
            match vouch_from_api(operator, request).await.map(|reply| self.usable(request, reply)) {
                Ok(usable) => match usable.await {
                    Ok(registration) => return Some(registration),
                    Err(e) => warn!(operator = %operator.origin, "the operator's vouch is unusable: {e:#}"),
                },
                Err(e) => warn!(operator = %operator.origin, "the operator did not vouch for the standby key: {e:#}"),
            }
        }
        let members = self.turn_members().await.unwrap_or_default();
        let ask = Request::Vouch { request: Box::new(request.clone()) };
        for member in members.iter().filter(|member| member.candidate != self.candidate && !member.endpoint.is_empty())
        {
            match Tcp.ask(&member.endpoint, &ask).await {
                Ok(Reply::Vouch { reply }) => match self.usable(request, reply).await {
                    Ok(registration) => return Some(registration),
                    Err(e) => warn!(member = %member.candidate, "the member's vouch is unusable: {e:#}"),
                },
                Ok(other) => warn!(member = %member.candidate, "the member did not vouch: {}", describe(&other)),
                Err(e) => warn!(member = %member.candidate, "asking a member to vouch failed: {e:#}"),
            }
        }
        None
    }

    /// The registration `reply` makes for `request`, once its vouch recovers to its voucher and the
    /// schedule takes that voucher at the latest agreed block.
    /// Error cases:
    /// - the vouch does not recover to its voucher, the schedule does not take the voucher, or the
    ///   read fails
    async fn usable(&self, request: &VouchRequest, reply: VouchReply) -> anyhow::Result<StandbyRegistration> {
        let registration = StandbyRegistration::new(&self.domain, request, &reply)?;
        anyhow::ensure!(self.takes(registration.voucher).await?, "the schedule takes no vouch from {}", reply.voucher);
        Ok(registration)
    }

    /// Whether the schedule takes a vouch from `voucher` at the latest agreed block (`may_vouch`).
    /// Error cases:
    /// - the read fails
    async fn takes(&self, voucher: AccountId) -> anyhow::Result<bool> {
        let read: BoxFuture<'_, anyhow::Result<bool>> = Box::pin(async move {
            let block = self.chain.latest().await?;
            may_vouch(&block, &self.domain, voucher).await
        });
        read.await
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

pub(crate) mod certificate;
mod keep;
pub(super) use keep::Kept;
mod orphans;
mod registration;
pub(crate) mod renewal;
