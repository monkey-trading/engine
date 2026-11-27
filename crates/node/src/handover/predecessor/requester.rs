//! The holder's check that a successor is the enclave its operator answers for. Every check of the
//! successor's release reads public values, which an enclave running that release on another host
//! satisfies as well, so every request must carry a signature over its report data by the successor's
//! standby key (`member_digest`), which only its own enclave holds, and the holder takes that key only
//! when the schedule lists it as the turn's member's promotable standby key, or, while the schedule lists
//! none, when an admission names it, signed by the key that answers for the successor
//! (`handover::admission`): the turn's admin key, the admin key of the operator the registry names while
//! rotation is on, or the operator key this release's log records while rotation is off. Nothing admits
//! a successor by its release alone.

use anyhow::{Context, ensure};
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;
use tracing::info;

use crate::checkpoints::CheckpointChain;
use crate::handover::quorum::Held;
use crate::handover::{Predecessor, Request, member_digest};
use crate::log::CheckpointDomain;
use crate::tee::sealing::Release;

/// Who answers for a successor: whose it is, for what is logged and refused, the key whose admission
/// admits it, and the promotable standby key the schedule lists for it, if any.
pub(super) struct Answers {
    pub who: String,
    pub admin_key: AccountId,
    pub standby: Option<AccountId>,
}

impl<C: CheckpointChain> Predecessor<C> {
    /// Who answers for a successor that the vault's own release names, with no turn naming the operator,
    /// from `held`, what the custody quorum read at the checkpoint endpoint's block: while rotation is on,
    /// the operator the registry names, once it names `release`; while it is off, this release's log's
    /// operator key.
    /// Error cases:
    /// - rotation is on and the registry names no operator, or one running another release
    /// - rotation is off and the log's operator key cannot be read (`log_operator`)
    pub(super) async fn release_answers(&self, held: &Held, release: &Release) -> anyhow::Result<Answers> {
        if held.rotation {
            let named = held.named.context("rotation is on, but the vault names no registry")?;
            ensure!(
                named.release == release.id(),
                "the registry names release 0x{} for its operator, not the successor's 0x{}",
                hex::encode(named.release),
                hex::encode(release.id())
            );
            let who = "the registry's operator".to_owned();
            return Ok(Answers { who, admin_key: named.admin_key, standby: None });
        }
        let admin_key = self.log_operator().await?;
        Ok(Answers { who: "this log's operator".to_owned(), admin_key, standby: None })
    }

    /// The operator key this release's log records: from the sequencer while it runs, and once it has
    /// stopped from the key retained at the stop (`retain_operator`).
    /// Error cases:
    /// - the log records no operator key, or the sequencer is gone and none was retained
    async fn log_operator(&self) -> anyhow::Result<AccountId> {
        match self.query(|engine| engine.operator().map(|operator| operator.address)).await {
            Ok(Some(key)) => Ok(key),
            Ok(None) => anyhow::bail!("this release's log records no operator key to admit a successor by"),
            Err(e) => self.retained_operator.lock().expect("the retained operator lock").ok_or(e),
        }
    }

    /// Retains the log's operator key as sequencing stops, which answers for a successor once the
    /// sequencer is gone; a read that fails leaves what was retained before, none in a node, which
    /// refuses such a successor.
    pub(super) async fn retain_operator(&self) {
        if let Ok(key) = self.query(|engine| engine.operator().map(|operator| operator.address)).await {
            *self.retained_operator.lock().expect("the retained operator lock") = key;
        }
    }
}

/// Refuses `request` unless the standby key that signed its `report_data` (`member_digest`) is the
/// promotable standby key `answers` names, or, with none, an admission signed by `answers.admin_key`
/// names that key for `release`: the key is sealed in the successor's own enclave, so an enclave running
/// `release` on another host signs with another key, which neither the schedule nor the operator answers
/// for. A listed key alone answers while it is listed, so an earlier admission of another stops counting
/// once the candidate registers one.
/// Error cases:
/// - the request carries no standby signature, or one that does not parse or recover
/// - the schedule lists another promotable key, or lists none and no admission names the key for
///   `release`, or the admission's signer is not `answers.admin_key`
pub(super) fn ensure_requester(
    domain: CheckpointDomain,
    answers: Answers,
    (request, report_data): (&Request, &[u8; 64]),
    release: &Release,
) -> anyhow::Result<()> {
    let Answers { who, admin_key, standby } = answers;
    let signature = request
        .standby_signature
        .as_deref()
        .with_context(|| format!("the request carries no signature by a standby key, which {who} must answer for"))?;
    let signature: Signature = signature.parse().context("the request's standby signature is not a signature")?;
    let key = eip712::recover(&B256::from(member_digest(domain, report_data)), &signature)
        .context("the request's standby signature does not recover")?;
    if let Some(promotable) = standby {
        ensure!(
            key == promotable,
            "the request is signed by standby key {key}, but {who}'s promotable standby key is {promotable}, \
             which alone answers for it while the schedule lists it"
        );
        info!(%key, "{who}'s promotable standby key signed the request");
        return Ok(());
    }
    let admission = request.admission.as_ref().with_context(|| {
        format!(
            "the request is signed by standby key {key}, and the schedule lists no promotable standby key for {who}, \
             and no admission names it"
        )
    })?;
    ensure!(
        admission.names(key, release.id()),
        "the admission names standby key {} for release {}, not the request's {key} for 0x{}",
        admission.key,
        admission.release,
        hex::encode(release.id())
    );
    let signer = admission.signer(domain);
    ensure!(
        signer == Some(admin_key),
        "the admission naming standby key {key} is signed by {signer:?}, not by {who}'s key {admin_key}"
    );
    info!(%key, %admin_key, "{who}'s key admitted the standby key that signed the request");
    Ok(())
}
