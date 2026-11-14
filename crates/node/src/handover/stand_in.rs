//! A stand-in predecessor for tests outside the handover: it answers tail requests in the order its
//! script gives — the keys and a tail of the bytes it is given, listed and attested as a predecessor
//! lists and attests them, as a host feeding the copy a tail it cannot take would, or a refusal of
//! the tip before the stop — and refuses every request past the script.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sha2::{Digest as _, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::evidence::{self, Quoting};
use super::messages::{Challenge, Reply, Request};
use super::tail::{TAIL_FRAME_BYTES, TailListing, TailMiss, send_tail, tail_report_data};
use super::{keys_report_data, receive, seal_keys, send};
use crate::config::TeeProvider;
use crate::log::CheckpointDomain;
use crate::tee::MockAttestor;
use crate::tee::keys::KeySet;
use crate::tee::sealing::Platform;

/// How the stand-in answers one request.
#[derive(Clone)]
pub(crate) enum Answer {
    /// The keys, with these bytes as the tail past the request's tip.
    Tail(Vec<u8>),
    /// The tip refused before the stop, for this reason.
    Missed(TailMiss),
}

/// Serves the requests on `listener` until aborted, for `domain`, handing `keys` over: each in turn
/// as `script` says, and each past it with a refusal. Counts the requests in the returned counter.
pub(crate) fn serve_tail(
    listener: TcpListener,
    domain: CheckpointDomain,
    keys: KeySet,
    script: Vec<Answer>,
) -> (JoinHandle<()>, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let counted = asked.clone();
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let answer = script.get(counted.fetch_add(1, Ordering::SeqCst)).cloned();
            // A failed answer leaves the successor to fail its own way.
            let _ = respond(&mut stream, domain, &keys, answer).await;
        }
    });
    (task, asked)
}

/// Answers one request on `stream` as `answer` says, or with a refusal.
/// Error cases:
/// - the request does not arrive or parse, names no tip, or the keys cannot be sealed or attested
async fn respond(
    stream: &mut TcpStream,
    domain: CheckpointDomain,
    keys: &KeySet,
    answer: Option<Answer>,
) -> anyhow::Result<()> {
    let challenge = [0x5c; 32];
    send(stream, &Challenge { nonce: hex::encode(challenge) }).await?;
    let request: Request = receive(stream).await?;
    let tail = match answer {
        Some(Answer::Tail(tail)) => tail,
        Some(Answer::Missed(miss)) => {
            return send(stream, &Reply::Missed { reason: format!("the stand-in refuses the tip: {miss:?}"), miss })
                .await;
        }
        None => return send(stream, &Reply::Refused { reason: "the stand-in's script has ended".into() }).await,
    };
    let tip = request.tail.clone().ok_or_else(|| anyhow::anyhow!("the request names no tip"))?;
    let public_key = hex::decode(&request.public_key)?;
    let config: [u8; 32] = hex::decode(&request.config_sha256)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("the config hash is not 32 bytes"))?;
    let report_data = tail_report_data(domain, &challenge, &public_key, &config, &request.reads, &tip);
    let mut handed = keys.clone();
    handed.handover_tail = Some(TailListing {
        after: tip.seq,
        last_seq: tip.seq + 1,
        entries: 1,
        bytes: tail.len() as u64,
        sha256: hex::encode(Sha256::digest(&tail)),
        frames: u32::try_from(tail.chunks(TAIL_FRAME_BYTES).count())?,
    });
    let (encapped_key, ciphertext) = seal_keys(domain, &public_key, &report_data, &handed)?;
    let bound = keys_report_data(domain, &report_data, &encapped_key, &ciphertext);
    let platform = Platform { launch_digest: [0x11; 48], policy: 0, tcb: 0 };
    let quoting = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "https://kds.test" };
    let evidence = evidence::produce(&quoting, &MockAttestor, &platform, bound).await?;
    let keys = Reply::Keys { encapped_key: hex::encode(encapped_key), ciphertext: hex::encode(ciphertext), evidence };
    send(stream, &keys).await?;
    send_tail(stream, &tail).await
}
