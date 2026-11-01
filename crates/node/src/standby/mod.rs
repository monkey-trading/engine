//! A standby for the operator in effect: follows its signed stream into a local copy of the
//! published log, ready to take over when the registry's liveness rule promotes this node's
//! candidate. The copy is written in the log's own segment format, so a promoted standby starts
//! from it like any log.
//!
//! Following verifies every frame against the enclave address the operator's verified attestation
//! names (`[standby].enclave`): the host that carries the connection can delay, drop, or splice
//! bytes, but cannot forge a frame. Missed history is backfilled from the operator's public
//! archive on a cold start, and by the stream itself after a restart; the archive's bytes are
//! checked against its manifest, and every checkpoint in them against the engine's replay when
//! the standby resumes.
//!
//! A takeover starts when the registry's pending election names this node's admin key and the
//! vault has had no checkpoint for the liveness timeout (a normal election, the incumbent still
//! checkpointing, hands the keys over instead — #44). The standby then finalizes its copy: it
//! refreshes completed segments from the archive in case the last vault-accepted checkpoint
//! posted after the stream last delivered, requires the checkpoint the vault accepted last, and
//! truncates the copy after it, dropping the unsigned tail. It then exits: the operator restarts
//! the node without `--standby`, with `--new-key`, which serves only its attestation until the
//! timelock installs the registered enclave address, recovers the copy, and resumes.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use engine_core::types::AccountId;
use futures_util::{StreamExt, pin_mut};
use rand::Rng;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use crate::checkpoints::Takeover;

pub(super) mod archive;

use crate::config::{CustodyProvider, NodeConfig, StandbySection};
use crate::log::{LogEntry, LogWriter, MANIFEST, Manifest, Offset, SegmentedLog, offset_of, truncate_to_checkpoint};
use crate::telemetry::Telemetry;

/// How long one follow stint runs before the standby re-checks the chain for its promotion: the
/// takeover's worst-case delay after the liveness timeout passes.
const STINT: Duration = Duration::from_secs(15);
/// The most archive bytes one fetch reads, bounding memory for a manifest or segment.
pub(super) const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
/// The most segments one manifest may list.
pub(super) const MAX_ARCHIVE_LISTINGS: usize = 65_536;
/// How long one archive GET's connection or a WebSocket connect waits.
pub(super) const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one archive body may take end to end.
pub(super) const BODY_TIMEOUT: Duration = Duration::from_secs(300);

/// Runs the standby until the registry promotes this node's candidate, then finalizes the copy
/// and returns for a normal `--new-key` start.
/// Error cases:
/// - the config is not an EVM-custody standby, the copy cannot be opened, the archive bootstrap
///   fails, or the takeover's checkpoint is absent from the copy after an archive refresh
pub async fn run(cfg: NodeConfig, telemetry: Telemetry) -> anyhow::Result<()> {
    let standby = cfg.standby.clone().context("--standby needs a [standby] section")?;
    ensure!(
        cfg.custody.provider == CustodyProvider::Evm && cfg.checkpoints.is_some(),
        "standby mode needs [checkpoints] with EVM custody: it reads the vault and registry"
    );
    let Telemetry { metrics: _, alerts, pages_critical } = telemetry;
    if !pages_critical {
        warn!("no alerts.critical webhook: Critical alerts are only logged");
    }
    cfg.alerts.outbox.check_disk_location(&cfg.node.log_dir)?;
    let routing = alerts.spawn(cfg.alerts.episode_quiet, &cfg.alerts.outbox).await?;
    let outcome = serve(&cfg, &standby).await;
    routing.drain(Duration::from_secs(10)).await;
    outcome
}

/// Follows and takes over; alert routing is the caller's.
/// Error cases:
/// - see `run`
async fn serve(cfg: &NodeConfig, standby: &StandbySection) -> anyhow::Result<()> {
    let my_key: AccountId = cfg.operator()?.address;
    let chain = crate::handover::vault_reader(cfg)?.context("standby mode needs [checkpoints]")?;
    let archive = archive::archive_client(standby)?;
    // A cold start backfills completed segments from the archive; a restart resumes the stream
    // after its own last entry, which backfills anything in between.
    let dir = cfg.node.log_dir.as_path();
    if !dir.join(MANIFEST).exists() {
        archive::bootstrap_archive(&archive, standby, dir).await?;
        // A poisoned cold start must surface now, not when the operator is already gone:
        // replay what was backfilled; a fresh deployment with no checkpoint yet has nothing
        // to verify, which is fine.
        if let Err(e) = crate::sequencer::replay_path(dir) {
            warn!("the backfilled copy does not replay: {e:#}");
        }
    }
    let mut writer = open_copy(dir, cfg.node.segment_commands.max(1))?;
    info!(operator = %standby.operator, enclave = %standby.enclave, "standby following the operator in effect");
    let mut follow_failed = false;
    let mut signal_failed = false;
    loop {
        match chain.takeover(my_key).await {
            Ok(Some(takeover)) => {
                writer.flush().context("flushing the copy before the takeover")?;
                drop(writer);
                if let Err(e) = finalize(&archive, standby, dir, &takeover).await {
                    error!(alert = "standby_takeover_failed", "the takeover could not finalize: {e:#}");
                    return Err(e);
                }
                info!(
                    alert_resolved = "standby_promoted",
                    seq = takeover.seq,
                    "promoted: the copy ends at the latest published checkpoint the outgoing key \
                     signed; start this node without --standby, with --new-key, under a config \
                     naming the operator in effect's key, and have the timelock install this \
                     enclave's registered address"
                );
                return Ok(());
            }
            Ok(None) => {
                if signal_failed {
                    info!(alert_resolved = "standby_signal_failed", "reading the takeover signal works again");
                    signal_failed = false;
                }
            }
            Err(e) => {
                if !signal_failed {
                    error!(alert = "standby_signal_failed", "reading the takeover signal failed: {e:#}");
                }
                signal_failed = true;
                sleep(standby.poll_interval).await;
            }
        }
        // The writer's last entry is the resume point, so an error mid-stint never duplicates
        // appends: the next stint re-requests from it.
        let resume = writer.last_offset();
        if let Err(e) = follow(&mut writer, cfg, standby, resume, STINT).await {
            if !follow_failed {
                error!(alert = "standby_follow_failed", operator = %standby.operator, "following failed: {e:#}");
            }
            follow_failed = true;
            // A dead operator refuses connections at once; the poll interval bounds the retry
            // rate for as long as the liveness window lasts.
            sleep(standby.poll_interval).await;
        } else if follow_failed {
            info!(alert_resolved = "standby_follow_failed", "following works again");
            follow_failed = false;
        } else {
            // A host that closes every connection at once must not spin this loop.
            sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Opens the copy for appends, repairing an interrupted takeover first: a crash during
/// finalization leaves a `takeover.json` marker, and re-running the truncation is idempotent, so
/// the next start finishes the surgery before following again.
/// Error cases:
/// - the copy cannot be opened, and no takeover marker repairs it
fn open_copy(dir: &std::path::Path, segment_commands: u64) -> anyhow::Result<LogWriter> {
    let open = |dir: &std::path::Path| {
        let (manifest_tx, _) = tokio::sync::watch::channel(Manifest::default());
        SegmentedLog::open(dir, |_| Ok(()))?.into_writer(segment_commands, manifest_tx)
    };
    match open(dir) {
        Ok(writer) => Ok(writer),
        Err(e) => {
            let marker = dir.join("takeover.json");
            let bytes = std::fs::read(&marker).map_err(|_| e)?;
            let takeover: Takeover =
                serde_json::from_slice(&bytes).context("parsing the interrupted takeover marker")?;
            crate::log::truncate_to_checkpoint(dir, takeover.seq, &takeover.root)
                .context("repairing the interrupted takeover")?;
            std::fs::remove_file(&marker).context("removing the takeover marker")?;
            open(dir).context("opening the repaired copy")
        }
    }
}

/// One follow stint: opens a signed stream after `resume`, verifies every frame against the
/// operator's enclave address, and appends each entry to the copy, for up to `budget`. A closed,
/// failed, or ended connection returns to re-check the chain; the next stint opens a fresh
/// connection, which the verifier binds, so a spliced connection is never trusted across stints.
/// Error cases:
/// - the connection fails, a frame fails verification, the copy cannot be written, or an entry
///   does not follow the last one
async fn follow(
    writer: &mut LogWriter,
    cfg: &NodeConfig,
    standby: &StandbySection,
    resume: Option<Offset>,
    budget: Duration,
) -> anyhow::Result<Option<Offset>> {
    let mut challenge = [0u8; 32];
    rand::thread_rng().fill(&mut challenge);
    let from = resume.map_or_else(|| "from=0".to_owned(), |offset| format!("after={offset}"));
    let path_and_query = format!("/v1/stream?signed=1&challenge={}&{from}", hex::encode(challenge));
    let url = stream_url(&standby.operator, &path_and_query)?;
    let connector = tokio_tungstenite::Connector::Rustls(tls(standby)?);
    let connect = tokio_tungstenite::connect_async_tls_with_config(url.as_str(), None, false, Some(connector));
    let (mut stream, _) = timeout(FETCH_TIMEOUT, connect)
        .await
        .context("connecting to the stream timed out")?
        .with_context(|| format!("connecting to {url}"))?;
    let mut verifier = engine_wire::FrameVerifier::new(
        standby.enclave.0,
        engine_wire::Deployment {
            chain_id: cfg.custody.chain_id,
            vault: cfg.custody.vault.parse::<AccountId>().context("custody.vault")?.0,
        },
        engine_wire::Connection::new(challenge, &path_and_query),
    );
    let deadline = sleep(budget);
    pin_mut!(deadline);
    let mut last = resume;
    // Every exit path flushes: a later stint resumes the stream after the writer's last entry, so
    // an unflushed append would be skipped forever.
    let followed = async {
        loop {
            let frame = {
                let next = stream.next();
                futures_util::pin_mut!(next);
                match futures_util::future::select(&mut deadline, next).await {
                    futures_util::future::Either::Left(_) => break,
                    futures_util::future::Either::Right((message, _)) => {
                        let message = message.context("reading the stream")??;
                        let text = match message {
                            Message::Text(text) => text,
                            Message::Close(_) => break,
                            // The stream sends text frames; anything else is ignored, as the
                            // subscriber side of the API does with what a subscriber sends.
                            _ => continue,
                        };
                        verifier.verify_text(&text).context("a stream frame failed verification")?
                    }
                }
            };
            let value: serde_json::Value = serde_json::from_str(&frame.msg).context("a stream message is not JSON")?;
            if value.get("type").and_then(serde_json::Value::as_str) == Some("heartbeat") {
                continue;
            }
            let message: crate::sequencer::StreamMsg =
                serde_json::from_value(value).context("a stream message is not a log entry")?;
            let entry = LogEntry::from(message.body);
            let expected = offset_of(last, &entry).context("offsetting a stream entry")?;
            ensure!(
                message.offset == expected,
                "the stream skipped from {last:?} to {}: history is missing",
                message.offset
            );
            writer.append(&entry).context("appending a stream entry to the copy")?;
            writer.flush().context("flushing the copy")?;
            last = Some(expected);
        }
        anyhow::Ok(last)
    }
    .await;
    writer.sync().context("syncing the copy")?;
    followed
}

/// Finalizes the copy for the takeover: refreshes completed segments from the archive in case the
/// vault's last accepted checkpoint posted after the stream last delivered, then requires that
/// checkpoint and truncates the copy after it, dropping the unsigned tail.
/// Error cases:
/// - the archive refresh fails hard, or the copy holds no checkpoint at the vault's latest
async fn finalize(
    archive: &reqwest::Client,
    standby: &StandbySection,
    dir: &std::path::Path,
    takeover: &Takeover,
) -> anyhow::Result<()> {
    if let Err(e) = archive::refresh_archive(archive, standby, dir).await {
        warn!("refreshing the archive before the takeover failed: {e:#}");
    }
    // The whole copy must replay before any history is dropped: archive-fetched bytes were only
    // hash-checked against the server's own manifest, and this catches a forged prefix here
    // rather than after the timelock installs this enclave. Withdrawals verify against the
    // followed operator's attested enclave, the only key that signed this copy's payouts.
    crate::sequencer::replay_path_with_attested(dir, None, [standby.enclave].into_iter().collect())
        .context("the copy does not replay; refusing to truncate it")?;
    // The resume point is the latest checkpoint in the published log that the outgoing key
    // signed and that extends the vault's latest root: the anchor itself, then every later
    // checkpoint of the copy that continues the outgoing key's signed parent chain. Checkpoints
    // the vault never saw but the outgoing key signed stay: they are published history this
    // node continues, and dropping them could conflict with a relay of the same signatures.
    let resume = resume_checkpoint(dir, takeover)?;
    // The marker makes the surgery resumable: a crash from here is repaired on restart.
    let marker = dir.join("takeover.json");
    std::fs::write(
        &marker,
        serde_json::to_string(&Takeover { seq: resume.0, root: resume.1.clone(), signer: takeover.signer })
            .context("encoding the takeover marker")?,
    )
    .context("writing the takeover marker")?;
    std::fs::File::open(&marker)?.sync_all().context("syncing the takeover marker")?;
    truncate_to_checkpoint(dir, resume.0, &resume.1).context("truncating the copy at the takeover")?;
    std::fs::remove_file(dir.join("takeover.json")).context("removing the takeover marker")?;
    Ok(())
}

/// The latest published checkpoint the outgoing key signed that extends the vault's latest root:
/// the anchor checkpoint itself, then the newest later copy checkpoint whose signed parent is
/// the running tip. Every skipped or foreign-signed checkpoint between them breaks the chain, so
/// the result is the deepest unbroken continuation.
/// Error cases:
/// - the copy cannot be read, or holds no checkpoint at the anchor
fn resume_checkpoint(dir: &std::path::Path, takeover: &Takeover) -> anyhow::Result<(u64, String)> {
    let mut tip: Option<(u64, String)> = None;
    let mut found_anchor = false;
    SegmentedLog::open(dir, |entry| {
        if let LogEntry::Checkpoint(checkpoint) = &entry {
            let root = checkpoint.root.to_lowercase();
            match &tip {
                None if checkpoint.seq == takeover.seq && root == takeover.root.to_lowercase() => {
                    if checkpoint.signer != takeover.signer.to_string() {
                        bail!(
                            "the anchor checkpoint at seq {} is signed by {}, not the vault's {}",
                            checkpoint.seq,
                            checkpoint.signer,
                            takeover.signer
                        );
                    }
                    found_anchor = true;
                    tip = Some((checkpoint.seq, root));
                }
                Some((seq, parent_root)) if checkpoint.signer == takeover.signer.to_string() => {
                    if let Some(parent) = &checkpoint.parent
                        && parent.has_checkpoint
                        && parent.seq == *seq
                        && format!("0x{}", hex::encode(parent.root.0)) == *parent_root
                    {
                        tip = Some((checkpoint.seq, root));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })?;
    ensure!(found_anchor, "the copy holds no checkpoint at the vault's latest seq {}", takeover.seq);
    Ok(tip.expect("the anchor set the tip"))
}

/// The TLS config for the stream: the venue feeds' public roots, or a verifier that accepts any
/// certificate when the config says so. Frame signatures authenticate the stream's content.
fn tls(standby: &StandbySection) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    if !standby.danger_accept_invalid_certs {
        return crate::feeds::venue::tls_config();
    }
    warn!("standby.danger_accept_invalid_certs: the operator's TLS certificate is not verified");
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// A certificate verifier that accepts anything: development only, behind
/// `standby.danger_accept_invalid_certs`, which a production build refuses. It authenticates
/// nothing; the operator's enclave-keyed frame signatures do.
#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
        ]
    }
}

/// The WebSocket URL for the stream: the operator's origin with an `http(s)` scheme rewritten to
/// `ws(s)`.
/// Error cases:
/// - the origin is not a URL with an http(s) scheme
fn stream_url(origin: &str, path_and_query: &str) -> anyhow::Result<String> {
    let origin = origin.trim_end_matches('/');
    let url = if origin.starts_with("https://") {
        format!("wss://{}{path_and_query}", origin.strip_prefix("https://").unwrap())
    } else if origin.starts_with("http://") {
        format!("ws://{}{path_and_query}", origin.strip_prefix("http://").unwrap())
    } else {
        bail!("[standby].operator must be an http(s) URL, not {origin:?}");
    };
    Ok(url)
}

pub(super) fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

#[cfg(test)]
mod tests;
