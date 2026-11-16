//! How a node asks a member on the member link (`liveness::link`), and what it checks in a status
//! answer.

use alloy::primitives::{Address, B256};
use anyhow::{Context, bail};
use engine_core::types::AccountId;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::handover::{receive, send};
use crate::liveness::Status;
use crate::liveness::link::{ASK_TIMEOUT, Reply, Request};

/// How a node asks a member.
pub(crate) trait Asking: Send + Sync {
    /// The member at `endpoint`'s answer to `request`.
    /// Error cases:
    /// - the connection, the question or the answer fails or outruns `ASK_TIMEOUT`
    fn ask(&self, endpoint: &str, request: &Request) -> impl Future<Output = anyhow::Result<Reply>> + Send;
}

/// Asks over TCP.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Tcp;

impl Asking for Tcp {
    async fn ask(&self, endpoint: &str, request: &Request) -> anyhow::Result<Reply> {
        timeout(ASK_TIMEOUT, async {
            let mut stream = TcpStream::connect(endpoint).await.with_context(|| format!("connecting to {endpoint}"))?;
            send(&mut stream, request).await?;
            receive(&mut stream).await
        })
        .await
        .with_context(|| format!("asking {endpoint} timed out"))?
    }
}

/// The status `reply` carries, once it recovers to `member`'s standby key `key` over the digest for
/// `nonce` in the deployment `vault` on `chain_id`.
/// Error cases:
/// - the reply is no status, names another member, or does not recover to the key
pub(crate) fn verified_status(
    reply: Reply,
    (chain_id, vault): (u64, Address),
    member: AccountId,
    key: AccountId,
    nonce: &[u8; 32],
) -> anyhow::Result<Status> {
    let Reply::Status { member: named, status, signature } = reply else {
        bail!("the member answered no status: {}", describe(&reply));
    };
    anyhow::ensure!(named == member, "asked {member}, answered as {named}");
    let digest = B256::from(status.digest(chain_id, vault, member, nonce));
    anyhow::ensure!(
        engine_core::eip712::recover(&digest, &signature) == Some(key),
        "the status does not recover to {member}'s standby key {key}"
    );
    Ok(status)
}

/// A short account of a reply that was not the one asked for.
pub(crate) fn describe(reply: &Reply) -> String {
    match reply {
        Reply::Refused { reason } => format!("refused: {reason}"),
        Reply::Status { .. } => "a status".into(),
        Reply::Entries { entries } => format!("{} entries", entries.len()),
        Reply::Vouch { .. } => "a vouch".into(),
        Reply::Attestation { .. } => "an attestation".into(),
    }
}
