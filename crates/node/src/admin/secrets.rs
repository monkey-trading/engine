//! `engine admin deliver-secrets` and `secrets-status`: the operator's side of a delivery
//! (`crate::secrets`). Nothing leaves this process until the enclave's quote has passed against the
//! release manifest: its measurement, policy, and TCB, the config hash against the manifest and this
//! config's own file, and this connection's fresh nonce. Secrets come from a file, never the command
//! line, and this prints their names alone.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail, ensure};
use clap::Args;
use engine_verify::Tcb;
use k256::ecdsa::SigningKey;
use ring::rand::{SecureRandom, SystemRandom};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::admin::CALL_TIMEOUT;
use crate::clock::now_ms;
use crate::config::{NamedSecret, NodeConfig, SecretName, SecretUse, TeeProvider};
use crate::handover::evidence;
use crate::handover::{Evidence, REPLY_TIMEOUT, receive, send};
use crate::log::CheckpointDomain;
use crate::secrets::Bundle;
use crate::secrets::delivery::{self, Hello, Offer, Reply, Status, offer_report_data, receipt_report_data};
use crate::tee::sealing::Release;
use crate::verify::{ReleaseManifest, load_release};

/// The enclave a delivery or status read is for, and what its quote must prove.
#[derive(Args)]
pub struct PeerArgs {
    /// The enclave's delivery listener, `[tee.sealing.secrets].listen`, as `host:port`. The host
    /// carries this connection, so it is plain TCP: the quote and the encryption protect it.
    #[arg(long, value_name = "HOST:PORT")]
    to: String,
    /// The release manifest the enclave's quote must match, which must list the config's hash.
    #[arg(long)]
    release: PathBuf,
    /// An extra root certificate, ASCII PEM, pinning the quote's chain in addition to the compiled-in
    /// vendor roots. Test and private chains verify against this; release chains do not need it.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Accept a mock enclave, whose quote proves nothing: development only, refused in a `production`
    /// build. The manifest then checks only its stand-in measurement and the config's hash.
    #[arg(long)]
    allow_mock: bool,
}

/// A delivery: where it goes, and the bundle.
#[derive(Args)]
pub struct DeliverArgs {
    #[command(flatten)]
    peer: PeerArgs,
    /// The bundle, TOML: `version = <n>`, higher than the node's for a rotation, and a `[secrets]`
    /// table of the names the config reads and their values. It replaces the whole sealed bundle.
    #[arg(long)]
    secrets_file: PathBuf,
    /// Deliver a bundle lacking a secret the node cannot run without, which then holds at its next
    /// start until a bundle holds it.
    #[arg(long)]
    partial: bool,
}

/// A connection whose enclave has passed its quote.
struct Attested {
    stream: TcpStream,
    nonce: [u8; 32],
    domain: CheckpointDomain,
    release: Release,
    manifest: ReleaseManifest,
    public_key: Vec<u8>,
    status: Status,
    own: TeeProvider,
    root: Option<String>,
}

/// Delivers the bundle in `args`, signed by `key`, and prints the enclave's quoted receipt.
/// Error cases:
/// - the file is not a bundle, or holds what this config's fields do not read, or a malformed value
/// - it lacks a secret the node cannot run without, and `--partial` is not given
/// - the enclave cannot be reached, or fails its quote (see `connect`)
/// - the bundle is older than what the enclave attests it holds
/// - the enclave refuses it, or its receipt's quote does not bind the receipt on this connection
pub(super) async fn deliver(cfg: &NodeConfig, key: &SigningKey, args: &DeliverArgs) -> anyhow::Result<()> {
    deliver_unshown(cfg, key, args).await.map_err(unshown)
}

/// `deliver`, whose errors may quote what the host sent.
/// Error cases:
/// - see `deliver`
async fn deliver_unshown(cfg: &NodeConfig, key: &SigningKey, args: &DeliverArgs) -> anyhow::Result<()> {
    let bundle = Bundle::from_file(&args.secrets_file)?;
    let named = cfg.named_secrets()?;
    bundle.check(&named)?;
    let missing = bundle.missing(&named);
    let holds_for = lacking(&named, &missing, |usage| usage == SecretUse::Required);
    ensure!(
        args.partial || holds_for.is_empty(),
        "this bundle replaces the whole sealed one and lacks {}, without which the node holds at its next start: \
         complete it, or pass --partial",
        holds_for.join(", ")
    );
    let mut peer = connect(cfg, &args.peer).await?;
    print_status(&peer);
    if let Some(sealed) = peer.status.sealed_version {
        ensure!(
            bundle.version >= sealed,
            "the enclave attests it holds version {sealed}, newer than this bundle's {}",
            bundle.version
        );
    }
    let signed = bundle.sign(peer.domain, key);
    let message = delivery::encrypt(peer.domain, &peer.release, &peer.public_key, &peer.nonce, &signed)?;
    send(&mut peer.stream, &message).await?;
    let reply: Reply = timeout(REPLY_TIMEOUT, receive(&mut peer.stream))
        .await
        .context("no receipt in time: the bundle may have been sealed, and a retry with the same version is safe")?
        .context("no receipt: the bundle may have been sealed, and a retry with the same version is safe")?;
    let (receipt, evidence) = match reply {
        Reply::Sealed { receipt, evidence } => (receipt, evidence),
        Reply::Refused { reason } => bail!("the enclave refused the bundle: {}", shown(&reason)),
    };
    let bound = receipt_report_data(peer.domain, &peer.nonce, &message, &receipt);
    check_quote(peer.own, &evidence, &bound, &peer.manifest, peer.root.as_deref(), unix_now())
        .context("the receipt's quote does not verify, so nothing shows the bundle was sealed")?;
    ensure!(
        receipt.version == signed.bundle.version && receipt.missing == missing,
        "the enclave's quoted receipt is for version {} lacking {:?}, not this bundle",
        receipt.version,
        receipt.missing
    );
    println!("sealed: version {} ({} secrets)", receipt.version, signed.bundle.secrets.len());
    if receipt.already_sealed {
        println!("already sealed: this was a retry");
    }
    if !holds_for.is_empty() {
        println!("lacking, so the node holds at its next start: {}", holds_for.join(", "));
    }
    let off = lacking(&named, &receipt.missing, |usage| usage != SecretUse::Required);
    if !off.is_empty() {
        println!("lacking, so these integrations stay off: {}", off.join(", "));
    }
    if receipt.restart_required {
        println!("restart required: what runs keeps the secrets it started with until a restart");
    }
    Ok(())
}

/// Prints what the enclave attests it holds.
/// Error cases:
/// - the enclave cannot be reached, or fails its quote (see `connect`)
pub(super) async fn status(cfg: &NodeConfig, args: &PeerArgs) -> anyhow::Result<()> {
    print_status(&connect(cfg, args).await.map_err(unshown)?);
    Ok(())
}

/// `error` as this prints it: a parser's error quotes what the host sent, and the operator reads it
/// in a terminal.
fn unshown(error: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!(shown(&format!("{error:#}")))
}

/// The names in `missing` whose fields' use `applies` to, each once.
fn lacking(named: &[NamedSecret], missing: &[SecretName], applies: impl Fn(SecretUse) -> bool) -> Vec<String> {
    let mut names: Vec<String> = named
        .iter()
        .filter(|field| applies(field.usage) && missing.contains(&field.name))
        .map(|field| field.name.to_string())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// `text` from the connection, which the host can write to, as this prints it: bounded, with no
/// control character that could rewrite what the operator's terminal shows.
fn shown(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(1024).collect()
}

/// Prints an enclave's quoted status.
fn print_status(peer: &Attested) {
    let missing: Vec<String> = peer.status.missing.iter().map(ToString::to_string).collect();
    println!(
        "attested: release {}, sealed version {}, running version {}, lacking [{}]",
        peer.manifest.name,
        peer.status.sealed_version.map_or("none".to_owned(), |version| version.to_string()),
        peer.status.running_version.map_or("none".to_owned(), |version| version.to_string()),
        missing.join(", ")
    );
}

/// Connects to the enclave and checks the offer it answers a fresh nonce with.
/// Error cases:
/// - `--allow-mock` in a `production` build, or the manifest or a root cannot be loaded
/// - the connection fails, or the enclave stays silent
/// - the offer fails `check_offer`
async fn connect(cfg: &NodeConfig, args: &PeerArgs) -> anyhow::Result<Attested> {
    ensure!(!(args.allow_mock && cfg!(feature = "production")), "--allow-mock is refused in a production build");
    let manifest = load_release(&args.release)?;
    let root = args.root.as_deref().map(read_root).transpose()?;
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let own = if args.allow_mock { TeeProvider::Mock } else { TeeProvider::ConfigfsTsm };
    let mut nonce = [0u8; 32];
    SystemRandom::new().fill(&mut nonce).map_err(|_| anyhow::anyhow!("the RNG failed"))?;
    let mut stream = timeout(CALL_TIMEOUT, TcpStream::connect(&args.to))
        .await
        .with_context(|| format!("connecting to {} timed out", args.to))?
        .with_context(|| format!("connecting to {}", args.to))?;
    send(&mut stream, &Hello { nonce: hex::encode(nonce) }).await?;
    // The enclave may fetch its quote's vendor collateral first.
    let offer: Offer = timeout(REPLY_TIMEOUT, receive(&mut stream)).await.context("no offer in time")??;
    let public_key = check_offer(&offer, domain, &nonce, cfg.file_sha256, &manifest, own, root.as_deref(), unix_now())?;
    let release = Release { launch_digest: manifest.launch_digest, config_sha256: cfg.file_sha256 };
    let status = offer.status;
    Ok(Attested { stream, nonce, domain, release, manifest, public_key, status, own, root })
}

/// The one-time key an offer names, once its quote proves the offer came from an enclave running
/// this manifest's release under `config_sha256`, on this connection, with dates checked at `now`
/// in Unix seconds.
/// Error cases:
/// - the offer's key or config hash is malformed, or its config is not `config_sha256`, which the
///   manifest must list
/// - its quote does not bind the offer and `nonce`, or fails `check_quote`
#[allow(clippy::too_many_arguments)]
fn check_offer(
    offer: &Offer,
    domain: CheckpointDomain,
    nonce: &[u8; 32],
    config_sha256: [u8; 32],
    manifest: &ReleaseManifest,
    own: TeeProvider,
    root: Option<&str>,
    now: i64,
) -> anyhow::Result<Vec<u8>> {
    ensure!(
        offer.config_sha256 == hex::encode(config_sha256),
        "the enclave runs config {}, not this config's {}",
        shown(&offer.config_sha256),
        hex::encode(config_sha256)
    );
    ensure!(
        manifest.config_name(&config_sha256).is_some(),
        "release {} lists no config with this config's hash {}",
        manifest.name,
        hex::encode(config_sha256)
    );
    let public_key = hex::decode(&offer.public_key).context("the offered key is not hex")?;
    let bound = offer_report_data(domain, nonce, &public_key, &config_sha256, &offer.status);
    check_quote(own, &offer.evidence, &bound, manifest, root, now)?;
    Ok(public_key)
}

/// Checks `evidence` binds `report_data` and comes from `manifest`'s release: its measurement, its
/// guest policy, and a TCB at least the manifest's, with dates checked at `now` in Unix seconds.
/// `engine_verify` checks the same for an attestation; a delivery's quote binds other report data,
/// since its enclave may not hold its keys yet.
/// Error cases:
/// - the quote does not verify (see `evidence::verify`)
/// - the measurement, policy, or TCB does not match the release
fn check_quote(
    own: TeeProvider,
    evidence: &Evidence,
    report_data: &[u8; 64],
    manifest: &ReleaseManifest,
    root: Option<&str>,
    now: i64,
) -> anyhow::Result<()> {
    let peer = evidence::verify(own, evidence, report_data, Some(now), root)?;
    ensure!(
        peer.launch_digest == manifest.launch_digest,
        "the quote's launch measurement is not release {}'s",
        manifest.name
    );
    ensure!(
        peer.policy == manifest.policy,
        "the quote's guest policy {:#x} differs from release {}'s {:#x}",
        peer.policy,
        manifest.name,
        manifest.policy
    );
    if let Some(reported) = peer.tcb {
        let minimum: Tcb = manifest.minimum_tcb.into();
        ensure!(reported.meets(minimum), "the quote's reported TCB {reported:?} is below the release's {minimum:?}");
    }
    Ok(())
}

/// This machine's time, the operator's own, in Unix seconds.
fn unix_now() -> i64 {
    now_ms().div_euclid(1000)
}

/// The extra root at `path`.
/// Error cases:
/// - the file cannot be read
fn read_root(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

#[cfg(test)]
mod tests;
