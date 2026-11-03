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
//! The copy takes only history that extends it (`chain`): each stream resumes at the copy's last
//! command, which the operator must repeat exactly, and each later command must carry the enclave's
//! seal over the link that continues the copy. An operator whose host cut its log back and
//! restarted it fails that check at the first entry, or signed heartbeat, that shows it. The
//! standby then raises `operator_log_rollback`, appends nothing more, and keeps its copy as the
//! source for restoring what the operator lost.
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
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::checkpoints::Takeover;

pub(crate) mod archive;
mod chain;
mod follow;
use chain::{Chain, rollback_of};
use follow::follow;

use crate::config::{CustodyProvider, NodeConfig, StandbySection};
use crate::log::{
    CheckpointDomain, LogEntry, LogWriter, MANIFEST, Manifest, Offset, SegmentedLog, truncate_to_checkpoint,
};
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
pub async fn run(mut cfg: NodeConfig, mut telemetry: Telemetry, start: crate::KeyStart) -> anyhow::Result<()> {
    let standby = cfg.standby.clone().context("--standby needs a [standby] section")?;
    ensure!(
        cfg.custody.provider == CustodyProvider::Evm && cfg.checkpoints.is_some(),
        "standby mode needs [checkpoints] with EVM custody: it reads the vault and registry"
    );
    // A standby needs its own chain access, and an elected operator brings its own secrets.
    let delivered = crate::node::secrets::startup(&mut cfg, &mut telemetry, &start)
        .await
        .inspect_err(|e| error!("refusing to serve: {e:#}"))?;
    let Telemetry { metrics: _, alerts, pages_critical } = telemetry;
    if !pages_critical {
        warn!("no alerts.critical webhook: Critical alerts are only logged");
    }
    cfg.alerts.outbox.check_disk_location(&cfg.node.log_dir)?;
    let routing = alerts.spawn(cfg.alerts.episode_quiet, &cfg.alerts.outbox).await?;
    let outcome = match delivered.listener {
        Some(listener) => tokio::select! {
            outcome = serve(&cfg, &standby) => outcome,
            outcome = listener.run() => outcome,
        },
        None => serve(&cfg, &standby).await,
    };
    routing.drain(Duration::from_secs(10)).await;
    outcome
}

/// Follows and takes over; alert routing is the caller's.
/// Error cases:
/// - see `run`
async fn serve(cfg: &NodeConfig, standby: &StandbySection) -> anyhow::Result<()> {
    let my_key: AccountId = cfg.operator()?.address;
    let chain = crate::handover::vault_reader(cfg)?.context("standby mode needs [checkpoints]")?;
    let archive = archive::archive_client(standby.danger_accept_invalid_certs)?;
    // A cold start backfills completed segments from the archive; a restart resumes the stream at
    // its own last command, which backfills anything after it.
    let dir = cfg.node.log_dir.as_path();
    if !dir.join(MANIFEST).exists() {
        archive::bootstrap_archive(&archive, &origins(standby), dir).await?;
        // A poisoned cold start must surface now, not when the operator is already gone:
        // replay what was backfilled; a fresh deployment with no checkpoint yet has nothing
        // to verify, which is fine.
        if let Err(e) = crate::sequencer::replay_path(dir) {
            warn!("the backfilled copy does not replay: {e:#}");
        }
    }
    let domain =
        CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().context("custody.vault")? };
    let (mut writer, mut copied) =
        open_copy(dir, cfg.node.segment_commands.max(1), Chain::new(domain, standby.enclave))?;
    info!(operator = %standby.operator, enclave = %standby.enclave, "standby following the operator in effect");
    let mut follow_failed = false;
    let mut signal_failed = false;
    let mut rolled_back = false;
    loop {
        match chain.takeover(my_key).await {
            Ok(Some(takeover)) => {
                writer.flush().context("flushing the copy before the takeover")?;
                drop(writer);
                if let Err(e) = finalize(&archive, standby, dir, &takeover, copied).await {
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
        // The copy's last command is the resume point, so an error mid-stint never duplicates
        // appends: the next stint re-requests from it.
        let followed = follow(&mut writer, &mut copied, cfg, standby, STINT).await;
        // The stream itself worked: what it carried is another matter.
        match stint_action(&followed, rolled_back, copied.confirmed()) {
            StintAction::FollowFailed => {
                let error = followed.as_ref().unwrap_err();
                if !follow_failed {
                    error!(alert = "standby_follow_failed", operator = %standby.operator, "following failed: {error:#}");
                }
                follow_failed = true;
                // A dead operator refuses connections at once; the poll interval bounds the retry
                // rate for as long as the liveness window lasts.
                sleep(standby.poll_interval).await;
                continue;
            }
            StintAction::Quiet => {
                if std::mem::take(&mut follow_failed) {
                    info!(alert_resolved = "standby_follow_failed", operator = %standby.operator, "following works again");
                }
                // A host that closes every connection at once must not spin this loop.
                sleep(Duration::from_secs(1)).await;
            }
            StintAction::Rollback(evidence) => {
                if std::mem::take(&mut follow_failed) {
                    info!(alert_resolved = "standby_follow_failed", operator = %standby.operator, "following works again");
                }
                if !std::mem::replace(&mut rolled_back, true) {
                    error!(
                        alert = "operator_log_rollback",
                        operator = %standby.operator,
                        "{evidence}; the copy takes nothing more and is kept as the source to restore it from"
                    );
                }
                sleep(standby.poll_interval).await;
            }
            StintAction::Resolved => {
                if std::mem::take(&mut follow_failed) {
                    info!(alert_resolved = "standby_follow_failed", operator = %standby.operator, "following works again");
                }
                info!(
                    alert_resolved = "operator_log_rollback",
                    operator = %standby.operator,
                    "the operator's log holds the copy's history again"
                );
                rolled_back = false;
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// What one follow stint says about the two follow alerts.
#[derive(Debug, PartialEq)]
enum StintAction {
    /// The stream itself failed: raise (once) `standby_follow_failed`; nothing else changes.
    FollowFailed,
    /// The stream worked and carried nothing new about a rollback: resolve `standby_follow_failed`
    /// if raised; no other change.
    Quiet,
    /// The stream worked and showed a rollback: resolve `standby_follow_failed` if raised; raise
    /// (once per episode) `operator_log_rollback` with the evidence.
    Rollback(String),
    /// The stream worked and repeated the whole tail of a rolled-back copy: resolve both alerts.
    Resolved,
}

/// The alert decision for one stint: the stream's failure is its own alert, whatever the failure
/// carries; an Ok stint that repeated the whole tail resolves a rollback episode, and a quiet Ok
/// stint resolves nothing but `standby_follow_failed`.
fn stint_action(followed: &anyhow::Result<Option<Offset>>, rolled_back: bool, confirmed: bool) -> StintAction {
    match followed.as_ref().err().and_then(rollback_of) {
        None if followed.is_err() => StintAction::FollowFailed,
        Some(rollback) => StintAction::Rollback(rollback.to_string()),
        None if rolled_back && confirmed => StintAction::Resolved,
        None => StintAction::Quiet,
    }
}

/// Opens the copy for appends, with `empty` taking its entries as the copy's chain, repairing an
/// interrupted takeover first: a crash during finalization leaves a `takeover.json` marker, and
/// re-running the truncation is idempotent, so the next start finishes the surgery before
/// following again.
/// Error cases:
/// - the copy cannot be opened, or its links computed, and no takeover marker repairs it
fn open_copy(dir: &std::path::Path, segment_commands: u64, empty: Chain) -> anyhow::Result<(LogWriter, Chain)> {
    let open = |dir: &std::path::Path| {
        let (manifest_tx, _) = tokio::sync::watch::channel(Manifest::default());
        let mut copied = empty.clone();
        let log = SegmentedLog::open(dir, |entry| copied.record(entry))?;
        anyhow::Ok((log.into_writer(segment_commands, manifest_tx)?, copied))
    };
    match open(dir) {
        Ok(opened) => Ok(opened),
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

/// Finalizes the copy for the takeover: refreshes completed segments from the archive in case the
/// vault's last accepted checkpoint posted after the stream last delivered, taking only segments
/// that extend `copied`, then requires that checkpoint and truncates the copy after it, dropping
/// the unsigned tail.
/// Error cases:
/// - the archive refresh fails hard, or the copy holds no checkpoint at the vault's latest
async fn finalize(
    archive: &reqwest::Client,
    standby: &StandbySection,
    dir: &std::path::Path,
    takeover: &Takeover,
    mut copied: Chain,
) -> anyhow::Result<()> {
    let refreshed =
        archive::refresh_archive(archive, &origins(standby), dir, |staged| copied.extend_with(staged)).await;
    match refreshed.as_ref().err().map(|e| (e, rollback_of(e))) {
        Some((_, Some(rollback))) => error!(
            alert = "operator_log_rollback",
            operator = %standby.operator,
            "{rollback}; the archive's segments are not added to the copy"
        ),
        Some((e, None)) => warn!("refreshing the archive before the takeover failed: {e:#}"),
        None => {}
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

/// The archive origins in fetch order: the mirrors first, the operator itself last, since the
/// operator may be dead exactly when the archive matters.
pub(super) fn origins(standby: &StandbySection) -> Vec<String> {
    standby.mirrors.iter().cloned().chain(std::iter::once(standby.operator.clone())).collect()
}

#[cfg(test)]
mod tests;
