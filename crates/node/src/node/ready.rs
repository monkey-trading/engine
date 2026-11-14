//! What a member's start reads before it connects for its turn's tail (#593): its sealed
//! certificate, the registry's rotation switch, the checkpoint setup at the vault's finalized checkpoint with its anchor checked
//! against the replica, the MONKEY emission budget, the posted log heads, the generation the turn's
//! keys are sealed in, and a trusted time sample. The start after the confirmation reads none of
//! them again, so nothing between the handover and the member's first command reads the chain or
//! samples time; only the revalidation point sits between them.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context;
use engine_core::emissions::TokenUnits;
use engine_core::types::AccountId;
use tracing::{error, info};

use crate::checkpoints::ScheduledTurn;
use crate::checkpoints::setup::Setup;
use crate::config::NodeConfig;
use crate::log::Checkpoint;
use crate::log_heads::StartupHeads;
use crate::metrics::Metrics;
use crate::node::KeyStart;
use crate::node::keys::{Keys, attestor_for};
use crate::node::member::Replica;
use crate::node::preflight::emission_preflight;
use crate::sequencer::replay::LiveReplay;
use crate::tee::certificates::CertificateStore;
use crate::tee::clock::GuestClock;
use crate::tee::sealing::KeyStore;
use crate::tee::{AttestationBundle, attest_bundle};
use crate::tls::TlsIdentity;
use crate::trusted_time::Sample;

/// What a member read before it connected for its turn's tail.
pub(crate) struct Ready {
    /// The checkpoint setup at the vault's finalized checkpoint, with `[checkpoints]`.
    pub setup: Option<Setup>,
    /// The replica's checkpoint at the vault's finalized seq, which the vault authenticated.
    pub anchor: Option<Checkpoint>,
    /// The emission period budget the vault's MONKEY cap was derived from.
    pub emission_budget: Option<TokenUnits>,
    /// The posted log heads the start checks the copy against, with `[log_heads]`.
    pub heads: Option<StartupHeads>,
    /// The generation the turn's keys are sealed in, by turn, which its `TurnTaken` waits on.
    pub generations: BTreeMap<u64, u64>,
    /// A trusted time sample taken just before connecting, which admits the start's first stamps.
    pub sample: Sample,
    /// The sealed certificate, opened before connecting, with `[api.tls.acme]`: one that does not
    /// open refuses the turn before the incumbent stops.
    pub certificates: Option<Arc<CertificateStore>>,
    /// The promotion that made this member the operator, for a promoted start (#595): its
    /// checkpoints wait for the promotion's finality, and its reorg stops it.
    pub promoted: Option<crate::standby_key::chain::Promotion>,
}

/// A member's start: its replica, the keys its tail handover sealed, what it read before
/// connecting, and what the handover moved.
pub(crate) struct Start {
    pub replica: Replica,
    pub store: KeyStore,
    pub ready: Ready,
    pub moved: Moved,
}

/// What a tail handover moved: the bytes of the entries past the command at `after`, and how many of
/// them the copy took, the rest repeating entries it held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Moved {
    pub after: u64,
    pub entries: u64,
    pub bytes: u64,
}

impl Ready {
    /// Opens the sealed certificate the start serves, as a start under `start`'s flags does, then
    /// reads what the start needs from the chain for `signer`, the vault's signer whose key the
    /// handover brings, against `replica`, for `turn`'s keys sealed in `generation`, its member's
    /// registered hostname the one the certificate names, then takes the
    /// trusted time sample `sample` gives, last, so it is fresh when the start stamps under it.
    /// Error cases:
    /// - the sealed certificate does not open (see `keys::certificates`)
    /// - the registry's rotation switch is off, or names another API hostname (see `check_rotation`)
    /// - the checkpoint setup fails, the replica does not retain the vault's finalized checkpoint, or
    ///   the vault does not authenticate it (see `Poster::startup`)
    /// - the emission budget cannot be read or differs from the log's (see `emission_preflight`)
    /// - the posted log heads cannot be read, or no trusted time sample can be taken
    pub(crate) async fn read(
        cfg: &NodeConfig,
        start: &KeyStart,
        metrics: &Metrics,
        signer: AccountId,
        replica: &LiveReplay,
        (turn, generation): (&ScheduledTurn, u64),
        sample: impl Future<Output = anyhow::Result<Sample>>,
    ) -> anyhow::Result<Self> {
        let certificates = crate::node::keys::certificates(cfg, start)?;
        crate::node::handover::check_rotation_for(cfg, &mut false, Some(turn.candidate)).await?;
        let setup = crate::checkpoints::setup::connect_successor(cfg, signer, metrics).await?;
        let anchor = match setup.as_ref().and_then(|setup| setup.posted_seq) {
            Some(seq) => Some(replica.checkpoint_at(seq).cloned().with_context(|| {
                format!("the replica does not retain the vault's finalized checkpoint at seq {seq}")
            })?),
            None => None,
        };
        let emission_budget = match &setup {
            Some(setup) => {
                setup.poster.startup(anchor.as_ref()).await.context("checking the vault's anchor")?;
                Some(emission_preflight(&setup.poster, replica.engine().emissions()).await?)
            }
            None => None,
        };
        let heads = crate::log_heads::setup::read_heads(cfg, signer).await?;
        let generations = BTreeMap::from([(turn.turn, generation)]);
        let sample = sample.await.context("taking the trusted time sample the start stamps under")?;
        Ok(Self { setup, anchor, emission_budget, heads, generations, sample, certificates, promoted: None })
    }

    /// What a promoted member's start reads (#595), as `read` does for a handover, but at the vault's
    /// latest block, which names `signer`, the promoted standby key, before the promotion is final:
    /// the checkpoint setup's parent is the vault's latest accepted checkpoint, which the promotion
    /// fixed and the replica holds, and `candidate`'s registered hostname is the one the certificate
    /// names.
    /// Error cases:
    /// - as `read`, or the latest view does not name `signer`, or its checkpoint is not the one the
    ///   promotion fixed
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn read_promoted(
        cfg: &NodeConfig,
        start: &KeyStart,
        metrics: &Metrics,
        signer: AccountId,
        candidate: AccountId,
        replica: &LiveReplay,
        promotion: crate::standby_key::chain::Promotion,
        sample: impl Future<Output = anyhow::Result<Sample>>,
    ) -> anyhow::Result<Self> {
        let certificates = crate::node::keys::certificates(cfg, start)?;
        crate::node::handover::check_rotation_for(cfg, &mut false, Some(candidate)).await?;
        let setup = crate::checkpoints::setup::connect_promoted(cfg, signer, metrics).await?;
        let fixed = (promotion.checkpoint_seq > 0).then_some(promotion.checkpoint_seq);
        anyhow::ensure!(
            setup.as_ref().is_none_or(|setup| setup.posted_seq == fixed),
            "the vault's latest checkpoint is not the one the promotion fixed at seq {}",
            promotion.checkpoint_seq
        );
        let anchor = match fixed {
            Some(seq) => Some(replica.checkpoint_at(seq).cloned().with_context(|| {
                format!("the replica does not retain the checkpoint the promotion fixed at seq {seq}")
            })?),
            None => None,
        };
        let emission_budget = match &setup {
            Some(setup) => {
                let view = crate::checkpoints::View::Latest;
                setup.poster.startup_at(view, anchor.as_ref()).await.context("checking the vault's anchor")?;
                Some(emission_preflight(&setup.poster, replica.engine().emissions()).await?)
            }
            None => None,
        };
        let heads = crate::log_heads::setup::read_heads(cfg, signer).await?;
        let generations = BTreeMap::from([(promotion.turn, promotion.generation)]);
        let sample = sample.await.context("taking the trusted time sample the start stamps under")?;
        Ok(Self { setup, anchor, emission_budget, heads, generations, sample, certificates, promoted: Some(promotion) })
    }
}

/// The keys a member's tail handover sealed in `store`, attested: the start opens no other store and
/// reads nothing.
/// Error cases:
/// - with real attestation, the guest kernel does not time on the TSC (alert
///   `clocksource_unprotected`), the keys do not parse, or the attestor cannot quote
pub(super) fn keys(cfg: &NodeConfig, store: KeyStore) -> anyhow::Result<Keys> {
    let attestor = attestor_for(cfg.tee.provider, &GuestClock::default())
        .inspect_err(|e| error!(alert = "clocksource_unprotected", "refusing to serve: {e:#}"))?;
    let held = store.keys();
    let (enclave, tls) = (held.enclave_key()?, TlsIdentity::new(held.tls_key()?)?);
    let bundle = AttestationBundle {
        enclave_address: enclave.address(),
        config_sha256: cfg.file_sha256,
        tls_public_key_sha256: tls.public_key_sha256,
    };
    let attestation = Arc::new(attest_bundle(attestor.as_ref(), &bundle).context("attesting the enclave")?);
    info!(enclave = %enclave.address_hex(), generation = store.generation(), "the keys the tail handover sealed, attested");
    Ok(Keys { enclave: Arc::new(enclave), tls, store: Some(Arc::new(store)), attestor, attestation })
}

#[cfg(test)]
mod tests;
