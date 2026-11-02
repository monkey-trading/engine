//! The running release's side of moving the delivered secrets (`handover::secrets`).

use anyhow::{Context, anyhow, ensure};
use tokio::net::TcpStream;
use tracing::info;

use crate::checkpoints::CheckpointChain;
use crate::handover::evidence::{self, Quoting};
use crate::handover::predecessor::{Predecessor, refuse};
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
    /// - the requester fails `admit`: its quote, its policy and TCB, or the vault's current release
    /// - the registry has rotation on, so this release change is an election's or a promotion's, or
    ///   it cannot be read
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
        ensure!(
            !self.vault.rotation_enabled().await.context("reading the registry's rotation switch")?,
            "rotation is on, so the vault names this release through an election or a promotion: an elected operator \
             brings its own secrets"
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
