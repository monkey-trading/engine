//! A release node's delivery listener, as the operator's tool speaks to it.

use anyhow::{Context, bail};
use engine_core::types::AccountId;

use crate::handover::{receive, send};
use crate::log::CheckpointDomain;
use crate::secrets::delivery::{self, Hello, Offer, Receipt, Reply, Status};
use crate::secrets::tests::{bundle, operator_key};
use crate::sequencer::withdrawal_scenario::keys::ReleaseNode;

/// What the node logs once the delivery listener is up, with the address it bound.
const DELIVERY_UP: &str = "secrets delivery listener up";

impl ReleaseNode {
    /// A connection to the delivery listener that has read the enclave's offer, and its nonce.
    /// Error cases:
    /// - the log shows no delivery listener, or the exchange fails
    async fn offer(&self) -> anyhow::Result<(tokio::net::TcpStream, [u8; 32], Offer)> {
        let port = self.logged_port(&[DELIVERY_UP])?.context("the node logged no delivery listener")?;
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        let nonce = [0x5c; 32];
        send(&mut stream, &Hello { nonce: hex::encode(nonce) }).await?;
        let offer = receive(&mut stream).await?;
        Ok((stream, nonce, offer))
    }

    /// What this node's enclave reports it holds.
    /// Error cases:
    /// - see `offer`
    pub(super) async fn secrets_status(&self) -> anyhow::Result<Status> {
        Ok(self.offer().await?.2.status)
    }

    /// Delivers a bundle holding `webhook`, as the operator's tool does, and returns its receipt.
    /// Error cases:
    /// - see `offer`, or the enclave refuses it
    pub(super) async fn deliver(&self, webhook: &str) -> anyhow::Result<Receipt> {
        let (mut stream, nonce, offer) = self.offer().await?;
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId(self.vault.into()) };
        let signed = bundle(1, &[("alerts_warning_url", webhook)]).sign(domain, &operator_key());
        let message = delivery::encrypt(domain, &self.release, &hex::decode(offer.public_key)?, &nonce, &signed)?;
        send(&mut stream, &message).await?;
        match receive(&mut stream).await? {
            Reply::Sealed { receipt, .. } => Ok(receipt),
            Reply::Refused { reason } => bail!("the enclave refused the bundle: {reason}"),
        }
    }
}
