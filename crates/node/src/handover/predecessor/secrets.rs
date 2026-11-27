//! The running release's side of moving the delivered secrets (`handover::secrets`).

use anyhow::{Context, anyhow, ensure};
use tokio::net::TcpStream;
use tracing::info;

use crate::checkpoints::{CheckpointChain, View};
use crate::handover::evidence::{self, Quoting};
use crate::handover::predecessor::{Predecessor, refuse};
use crate::handover::quorum::page_disagreement;
use crate::handover::secrets::{hpke_info, reply_report_data, request_report_data};
use crate::handover::{Reply, Request, send};
use crate::secrets::delivery;

impl<C: CheckpointChain> Predecessor<C> {
    /// Answers a request for the secrets alone: sends them, or refuses with its reason. Sequencing
    /// goes on either way.
    pub(super) async fn serve_secrets(
        &self,
        stream: &mut TcpStream,
        request: &Request,
        challenge: &[u8; 32],
        public_key: &[u8],
        config_sha256: [u8; 32],
    ) {
        if let Err(e) = self.send_secrets(stream, request, challenge, public_key, config_sha256).await {
            refuse(stream, e).await;
        }
    }

    /// Sends the secrets to a release the vault names through an approved upgrade.
    /// Error cases:
    /// - the requester fails `admit`: its quote, its CPU model and TCB, its policy, or the vault's
    ///   current release
    /// - the registry has rotation on, so this release change is an election's or a promotion's, by the
    ///   checkpoint endpoint or the custody quorum, at finality or at the latest block, or a read fails
    /// - this release holds no secrets, or encrypting, quoting, or sending fails
    async fn send_secrets(
        &self,
        stream: &mut TcpStream,
        request: &Request,
        challenge: &[u8; 32],
        public_key: &[u8],
        config_sha256: [u8; 32],
    ) -> anyhow::Result<()> {
        let requested = request_report_data(self.domain, challenge, public_key, &config_sha256);
        let release = self.admit(request, &requested, config_sha256).await?;
        // At finality, the state that named this release, and at the latest block, so a switch thrown
        // inside the finality window moves nothing either way; by the checkpoint endpoint, a single
        // provider, and by the custody quorum at the endpoint's block and at its own head.
        let finalized = self.vault.observe(View::Finalized, &[]).await.context("reading the vault at finality")?;
        let at_finality = self.vault.rotation_enabled_at(&finalized).await.context("reading the rotation switch")?;
        let latest = self.vault.rotation_enabled().await.context("reading the rotation switch")?;
        let majority = self.quorum.rotation(&finalized).await;
        let (held, head) = majority.context("reading the rotation switch through the custody quorum")?;
        // The same block: a difference there is the endpoint's fault, where the latest blocks may differ.
        if at_finality == held {
            info!(
                alert_resolved = "checkpoint_endpoint_disagrees",
                "the custody quorum reads the rotation switch as the checkpoint endpoint does"
            );
        } else {
            page_disagreement(format_args!(
                "the checkpoint endpoint reads the rotation switch {at_finality} at finality, but the custody quorum reads \
                 it {held} at the same block"
            ));
        }
        ensure!(
            !at_finality && !latest && !held && !head,
            "rotation is on, so the vault names this release through an election or a promotion: an elected operator \
             brings its own secrets (the checkpoint endpoint reads it {at_finality} at finality and {latest} at the \
             latest block, the custody quorum {held} and {head})"
        );
        let signed = self
            .secrets
            .as_ref()
            .and_then(|store| store.current())
            .ok_or_else(|| anyhow!("this release holds no delivered secrets"))?;
        let (encapped_key, ciphertext) =
            delivery::seal(&hpke_info(self.domain), public_key, &requested, &signed.to_bytes()?)?;
        let bound = reply_report_data(self.domain, &requested, &encapped_key, &ciphertext);
        let quoting =
            Quoting { provider: self.provider, processor: self.processor.as_deref(), kds_base: &self.kds_base };
        let evidence = evidence::produce(&quoting, self.attestor.as_ref(), &self.store.platform(), bound).await?;
        let reply =
            Reply::Secrets { encapped_key: hex::encode(encapped_key), ciphertext: hex::encode(ciphertext), evidence };
        send(stream, &reply).await?;
        info!(
            successor_release = %hex::encode(release.id()),
            version = signed.bundle.version,
            "moved the delivered secrets to the release the vault names"
        );
        Ok(())
    }
}
