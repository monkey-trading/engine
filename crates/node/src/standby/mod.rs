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
use tracing::{info, warn};

use crate::checkpoints::Takeover;
use crate::config::{CustodyProvider, NodeConfig, StandbySection};
use crate::log::{
    LogEntry, LogWriter, MANIFEST, Manifest, Offset, SegmentInfo, SegmentedLog, offset_of, truncate_to_checkpoint,
};
use crate::telemetry::Telemetry;

/// How long one follow stint runs before the standby re-checks the chain for its promotion: the
/// takeover's worst-case delay after the liveness timeout passes.
const STINT: Duration = Duration::from_secs(15);
/// The most archive bytes one fetch reads, bounding memory for a manifest or segment.
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
/// How long one archive GET or WebSocket connect waits.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

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
    let archive = archive_client(standby)?;
    // A cold start backfills completed segments from the archive; a restart resumes the stream
    // after its own last entry, which backfills anything in between.
    let dir = cfg.node.log_dir.as_path();
    if !dir.join(MANIFEST).exists() {
        bootstrap_archive(&archive, standby, dir).await?;
    }
    let (manifest_tx, _) = tokio::sync::watch::channel(Manifest::default());
    let mut writer = SegmentedLog::open(dir, |_| Ok(()))?
        .into_writer(cfg.node.segment_commands.max(1), manifest_tx)
        .context("opening the standby copy for appends")?;
    info!(operator = %standby.operator, enclave = %standby.enclave, "standby following the operator in effect");
    loop {
        match chain.takeover(my_key).await {
            Ok(Some(takeover)) => {
                writer.flush().context("flushing the copy before the takeover")?;
                drop(writer);
                finalize(&archive, standby, dir, &takeover).await?;
                info!(
                    alert_resolved = "standby_promoted",
                    seq = takeover.seq,
                    "promoted: the copy ends at the vault's latest checkpoint; start this node without --standby, \
                     with --new-key, and have the timelock install this enclave's registered address"
                );
                return Ok(());
            }
            Ok(None) => {}
            Err(e) => warn!("reading the takeover signal failed: {e:#}"),
        }
        // The writer's last entry is the resume point, so an error mid-stint never duplicates
        // appends: the next stint re-requests from it.
        let resume = writer.last_offset();
        if let Err(e) = follow(&mut writer, cfg, standby, resume, STINT).await {
            warn!(operator = %standby.operator, "following failed: {e:#}");
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
    if let Err(e) = refresh_archive(archive, standby, dir).await {
        warn!("refreshing the archive before the takeover failed: {e:#}");
    }
    ensure!(
        copy_holds_checkpoint(dir, takeover.seq, &takeover.root)?,
        "the copy holds no checkpoint at seq {} with root {}: the vault accepted a checkpoint the \
         operator never published to the stream or archive; the standby cannot take over",
        takeover.seq,
        takeover.root
    );
    truncate_to_checkpoint(dir, takeover.seq, &takeover.root).context("truncating the copy at the takeover")
}

/// Whether the copy holds the checkpoint at `seq` with `root`.
/// Error cases:
/// - the copy cannot be read
fn copy_holds_checkpoint(dir: &std::path::Path, seq: u64, root: &str) -> anyhow::Result<bool> {
    let mut found = false;
    SegmentedLog::open(dir, |entry| {
        if let LogEntry::Checkpoint(checkpoint) = &entry {
            found |= checkpoint.seq == seq && checkpoint.root == root;
        }
        Ok(())
    })?;
    Ok(found)
}

/// A cold start: fetches the published manifest and every completed segment it lists into the
/// empty copy, checking each file against its listing, and writes the manifest. A missing
/// manifest (a fresh deployment) leaves the copy empty for the stream to fill from seq 0.
/// Error cases:
/// - a fetch fails, exceeds the bound, or a file differs from its listing
async fn bootstrap_archive(
    archive: &reqwest::Client,
    standby: &StandbySection,
    dir: &std::path::Path,
) -> anyhow::Result<()> {
    let listings = match fetch_manifest(archive, standby).await {
        Ok(listings) => listings,
        Err(e) => match e.downcast::<reqwest::Error>() {
            Ok(reqwest) if reqwest.status() == Some(reqwest::StatusCode::NOT_FOUND) => return Ok(()),
            Ok(reqwest) => return Err(reqwest).context("fetching the published manifest"),
            Err(e) => return Err(e).context("fetching the published manifest"),
        },
    };
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut text = String::new();
    for listed in &listings {
        let bytes = fetch_archive_bytes(archive, standby, &listed.file).await?;
        let actual = sha256(&bytes);
        ensure!(
            bytes.len() as u64 == listed.bytes && actual == listed.sha256,
            "archive segment {} differs from its listing",
            listed.file
        );
        std::fs::write(dir.join(&listed.file), bytes).with_context(|| format!("writing {}", listed.file))?;
        text.push_str(&serde_json::to_string(listed).context("encoding a manifest line")?);
        text.push('\n');
    }
    std::fs::write(dir.join(MANIFEST), text).with_context(|| format!("writing {MANIFEST}"))?;
    info!(segments = listings.len(), "backfilled the copy from the operator's archive");
    Ok(())
}

/// Best-effort refresh of completed segments the copy lacks, run once a takeover is promoted:
/// the checkpoint the vault accepted last may post after the stream last delivered.
/// Error cases:
/// - the manifest cannot be fetched or a listed file differs from its listing
async fn refresh_archive(
    archive: &reqwest::Client,
    standby: &StandbySection,
    dir: &std::path::Path,
) -> anyhow::Result<()> {
    let listings = fetch_manifest(archive, standby).await.context("fetching the published manifest")?;
    let mut text = String::new();
    for listed in &listings {
        let path = dir.join(&listed.file);
        if path.exists()
            && std::fs::metadata(&path).is_ok_and(|meta| meta.len() == listed.bytes)
            && sha256(&std::fs::read(&path)?) == listed.sha256
        {
            text.push_str(&serde_json::to_string(listed).context("encoding a manifest line")?);
            text.push('\n');
            continue;
        }
        let bytes = fetch_archive_bytes(archive, standby, &listed.file).await?;
        ensure!(
            bytes.len() as u64 == listed.bytes && sha256(&bytes) == listed.sha256,
            "archive segment {} differs from its listing",
            listed.file
        );
        std::fs::write(path, &bytes).with_context(|| format!("writing {}", listed.file))?;
        text.push_str(&serde_json::to_string(listed).context("encoding a manifest line")?);
        text.push('\n');
    }
    std::fs::write(dir.join(MANIFEST), text).with_context(|| format!("writing {MANIFEST}"))?;
    Ok(())
}

/// The archive's manifest: one segment listing a line, oldest first.
/// Error cases:
/// - the fetch fails or a line is not a listing
async fn fetch_manifest(archive: &reqwest::Client, standby: &StandbySection) -> anyhow::Result<Vec<SegmentInfo>> {
    let bytes = fetch_archive_bytes(archive, standby, MANIFEST).await?;
    bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.trim_ascii().is_empty())
        .map(|line| serde_json::from_slice(line).context("parsing a manifest line"))
        .collect()
}

/// One archive GET, bounded in bytes and time.
/// Error cases:
/// - the fetch fails, exceeds the bound, or the body exceeds `MAX_ARCHIVE_BYTES`
async fn fetch_archive_bytes(
    archive: &reqwest::Client,
    standby: &StandbySection,
    file: &str,
) -> anyhow::Result<Vec<u8>> {
    let url = format!("{}/v1/archive/{file}", standby.operator.trim_end_matches('/'));
    let sent = timeout(FETCH_TIMEOUT, archive.get(&url).send()).await.context("archive fetch timed out")??;
    let mut response = sent.error_for_status().with_context(|| format!("fetching {url}"))?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context("reading the archive")? {
        ensure!(bytes.len() as u64 + chunk.len() as u64 <= MAX_ARCHIVE_BYTES, "archive file {file} exceeds the bound");
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// The TLS client config for archive fetches: public roots, or no verification when the config
/// says so. The stream's frame signatures are the authenticity boundary either way.
fn archive_client(standby: &StandbySection) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    if standby.danger_accept_invalid_certs {
        warn!("standby.danger_accept_invalid_certs: the operator's TLS certificate is not verified");
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().context("building the archive client")
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

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

#[cfg(test)]
mod tests;
