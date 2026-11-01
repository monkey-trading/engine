//! The enclave's keys for this start, and their attestation. With `[tee.sealing]` they are the keys
//! sealed for this release, new ones only on the operator's `--new-key`, or the previous release's,
//! taken over with `--predecessor`; with none of these the node refuses to start. With
//! `[checkpoints]`, the vault's current release must be this one before it opens or seals keys,
//! which it does in the vault's generation of the release, and the predecessor checks the same
//! before it hands them over; a release governance names again takes them afresh. A mock
//! configuration without sealing uses a fresh key, or the published one, for this process only.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use tracing::{error, info, warn};

use crate::checkpoints::CheckpointChain;
use crate::config::{NodeConfig, TeeProvider};
use crate::handover::{current_release, ensure_named, vault_reader};
use crate::log::Checkpoint;
use crate::tee::keys::KeySet;
use crate::tee::sealing::{KeyStore, MockSealer, Release, Sealer, file_for};
use crate::tee::{Attestation, AttestationBundle, Attestor, ConfigfsTsm, EnclaveKey, MockAttestor, attest_bundle};
use crate::tls::TlsIdentity;
use engine_core::types::AccountId;

/// The operator's one-off choices for a start of a release that has no sealed keys.
#[derive(Clone, Debug, Default)]
pub struct KeyStart {
    /// Generate and seal new keys, which the vault must then register as its signer.
    pub new_key: bool,
    /// Take the keys from the release whose handover listener is at this address.
    pub predecessor: Option<String>,
    /// The mock TEE's stand-in for the launch measurement, from which its public stand-in for
    /// sealing derives the key; a real platform measures the release itself.
    pub mock_launch_digest: Option<[u8; 48]>,
}

/// Parses `--mock-launch-digest`: 48 bytes of hex, with or without `0x`.
/// Error cases:
/// - the text is not hex, or not 48 bytes
pub fn parse_launch_digest(text: &str) -> Result<[u8; 48], String> {
    let bytes = hex::decode(text.strip_prefix("0x").unwrap_or(text)).map_err(|e| format!("not hex: {e}"))?;
    bytes.try_into().map_err(|bytes: Vec<u8>| format!("{} bytes, not a 48-byte launch measurement", bytes.len()))
}

/// The enclave's keys and their attestation.
pub(super) struct Keys {
    pub enclave: Arc<EnclaveKey>,
    pub tls: TlsIdentity,
    /// The sealed keys, with `[tee.sealing]`.
    pub store: Option<Arc<KeyStore>>,
    pub attestor: Box<dyn Attestor>,
    pub attestation: Arc<Attestation>,
}

/// This start's keys, attested.
/// Error cases:
/// - the keys cannot be obtained (see `obtain`), or the attestor cannot produce a quote
pub(super) async fn attested(cfg: &NodeConfig, start: &KeyStart) -> anyhow::Result<Keys> {
    let attestor: Box<dyn Attestor> = match cfg.tee.provider {
        TeeProvider::Mock => {
            warn!("mock TEE: attestation reports are not backed by hardware");
            Box::new(MockAttestor)
        }
        TeeProvider::ConfigfsTsm => Box::new(ConfigfsTsm::default()),
    };
    let (enclave, tls, store) = obtain(cfg, start, attestor.as_ref())
        .await
        .inspect_err(|e| error!(alert = "enclave_key_failed", "refusing to serve: {e:#}"))?;
    if cfg.tee.use_published_mock_key {
        warn!("using the public development signing key: mock attestation only");
    }
    let bundle = AttestationBundle {
        enclave_address: enclave.address(),
        config_sha256: cfg.file_sha256,
        tls_public_key_sha256: tls.public_key_sha256,
    };
    let attestation = Arc::new(attest_bundle(attestor.as_ref(), &bundle).context("attesting the enclave")?);
    info!(
        enclave = %enclave.address_hex(),
        config_sha256 = %attestation.config_sha256,
        tls_public_key_sha256 = %attestation.tls_public_key_sha256,
        provider = %attestation.provider,
        sealed = store.is_some(),
        "enclave, config, and TLS key attested"
    );
    Ok(Keys { enclave: Arc::new(enclave), tls, store: store.map(Arc::new), attestor, attestation })
}

/// The keys `start` and the config call for (see the module docs), reading the vault `[checkpoints]`
/// names, if any (see `obtain_with`).
/// Error cases:
/// - the vault's client cannot be built, or see `obtain_with`
async fn obtain(
    cfg: &NodeConfig,
    start: &KeyStart,
    attestor: &dyn Attestor,
) -> anyhow::Result<(EnclaveKey, TlsIdentity, Option<KeyStore>)> {
    obtain_with(cfg, start, attestor, vault_reader(cfg)?.as_ref()).await
}

/// `obtain`, reading the vault through `vault`, when the config names one.
/// Error cases:
/// - a flag is given without `[tee.sealing]`, or real attestation runs without it
/// - the stand-in measurement is missing under the mock TEE, or given under real attestation
/// - the vault cannot be read, or, without `--predecessor`, its current release is not this one
/// - the sealed keys cannot be opened, or a flag is given beside them
/// - the release has no keys sealed in the vault's generation of it and no flag, or new keys cannot
///   be sealed, or the handover fails
async fn obtain_with(
    cfg: &NodeConfig,
    start: &KeyStart,
    attestor: &dyn Attestor,
    vault: Option<&impl CheckpointChain>,
) -> anyhow::Result<(EnclaveKey, TlsIdentity, Option<KeyStore>)> {
    let Some(sealing) = &cfg.tee.sealing else {
        ensure!(
            !start.new_key && start.predecessor.is_none() && start.mock_launch_digest.is_none(),
            "--new-key, --predecessor, and --mock-launch-digest need [tee.sealing]"
        );
        ensure!(
            cfg.tee.provider == TeeProvider::Mock,
            "real attestation needs [tee.sealing]: without it every start has a signer the vault does not name"
        );
        return Ok((EnclaveKey::for_config(&cfg.tee)?, TlsIdentity::generate()?, None));
    };
    let sealer = sealer(cfg.tee.provider, start)?;
    let release = Release { launch_digest: sealer.platform()?.launch_digest, config_sha256: cfg.file_sha256 };
    let dir = sealing.dir.as_path();
    // Seal or open keys only under a folded release change: while `syncedRelease` trails
    // `currentRelease()`, anyone's `syncRelease` would raise the generation and supersede a file
    // sealed now, so the promoter must fold the change first (runbook), and this start waits,
    // re-reading the release from the same observation that shows the fold, so the generation
    // always matches the release it sealed under.
    let current = match vault {
        Some(vault) if vault.reads_release_fold() => {
            let folded = tokio::time::timeout(std::time::Duration::from_secs(60), async {
                loop {
                    let at = vault.observe(crate::checkpoints::View::Finalized, &[]).await?;
                    let release = vault.current_release(&at).await?;
                    if vault.synced_release(&at).await? == release.id {
                        return anyhow::Ok(release);
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            })
            .await
            .context(
                "the vault never folded the release change into its generation; have the promoter call syncRelease",
            )??;
            Some(folded)
        }
        vault => match vault {
            Some(vault) => Some(current_release(vault).await?),
            None => None,
        },
    };
    let generation = current.map_or(0, |current| current.generation);
    let sealed = file_for(dir, &release, generation).exists();
    ensure!(
        !sealed || (!start.new_key && start.predecessor.is_none()),
        "keys are already sealed for this release in {}; start without --new-key or --predecessor",
        dir.display()
    );
    // A predecessor checks this itself before it hands its keys over.
    if let (Some(current), None) = (current, &start.predecessor) {
        ensure_named(current, &release).context(
            "refusing this release's keys: the vault names one release at a time, and governance has not \
             made this one current, or has since superseded it with an upgrade or config change",
        )?;
    }
    let store = if sealed {
        let store = KeyStore::open(dir, sealer.as_ref(), cfg.file_sha256, generation)?
            .with_context(|| format!("this release's sealed keys left {}", dir.display()))?;
        info!(dir = %dir.display(), generation, "opened this release's sealed keys");
        store
    } else if start.new_key {
        warn!(dir = %dir.display(), "sealing new keys, which the vault must register before this node signs");
        KeyStore::create(dir, sealer.as_ref(), cfg.file_sha256, generation, KeySet::generate()?)?
    } else if let Some(address) = &start.predecessor {
        let vault = vault.context("--predecessor needs [checkpoints]: the vault names the key")?;
        crate::handover::take(cfg, address, sealer.as_ref(), attestor, vault).await?
    } else {
        bail!(refusal(dir, generation))
    };
    let keys = store.keys();
    Ok((keys.enclave_key()?, TlsIdentity::new(keys.tls_key()?)?, Some(store)))
}

/// The platform's sealing for `provider`.
/// Error cases:
/// - the mock TEE has no stand-in measurement, real attestation is given one, or SEV-SNP sealing is
///   asked for off Linux
fn sealer(provider: TeeProvider, start: &KeyStart) -> anyhow::Result<Box<dyn Sealer>> {
    if provider == TeeProvider::Mock {
        let launch_digest = start
            .mock_launch_digest
            .context("the mock TEE's stand-in for sealing needs --mock-launch-digest, its stand-in measurement")?;
        warn!(launch_digest = %hex::encode(launch_digest), "mock TEE: the keys' sealing is a public stand-in, not sealing");
        return Ok(Box::new(MockSealer { launch_digest }));
    }
    ensure!(
        start.mock_launch_digest.is_none(),
        "--mock-launch-digest is for the mock TEE: the platform measures this release"
    );
    #[cfg(target_os = "linux")]
    return Ok(Box::new(crate::tee::snp::SnpSealer));
    #[cfg(not(target_os = "linux"))]
    bail!("SEV-SNP sealing needs a Linux guest")
}

/// On a takeover's first start under the new signer, cuts the log to the vault's finalized
/// checkpoint. The standby's copy kept the outgoing operator's later published checkpoints past
/// the vault's latest; their signatures are unpostable now, and writable recovery admits only
/// what the vault accepted, so the unposted tail goes before recovery opens the log. A
/// `takeover.json` marker makes the cut resumable: a crash mid-cut is repaired on the next start.
/// Error cases:
/// - the chain or log cannot be read, or the cut cannot complete
pub(super) async fn cut_to_finalized_anchor(cfg: &NodeConfig, key: &Arc<EnclaveKey>) -> anyhow::Result<()> {
    use crate::checkpoints::{CheckpointChain, View};
    let Some(chain) = crate::handover::vault_reader(cfg)? else { return Ok(()) };
    let dir = cfg.node.log_dir.as_path();
    let marker = dir.join("takeover.json");
    let repair = |e: &anyhow::Error| -> anyhow::Error {
        match std::fs::read(&marker) {
            Ok(bytes) => match serde_json::from_slice::<crate::checkpoints::Takeover>(&bytes) {
                Ok(takeover) => {
                    if let Err(cut) = crate::log::truncate_to_checkpoint(dir, takeover.seq, &takeover.root) {
                        return anyhow::anyhow!("{e:#}; and repairing the interrupted cut failed: {cut:#}");
                    }
                    let _ = std::fs::remove_file(&marker);
                    anyhow::anyhow!("{e:#}; repaired the interrupted cut: start again")
                }
                Err(_) => anyhow::anyhow!("{e:#}"),
            },
            Err(_) => anyhow::anyhow!("{e:#}"),
        }
    };
    let state = chain.state(View::Finalized).await.map_err(|e| repair(&e))?;
    if state.signer != AccountId(key.address()) {
        return Ok(());
    }
    let Some((anchor_seq, anchor_root)) = state.checkpoint else { return Ok(()) };
    // Only a tail another key signed is cut: a node's own unposted checkpoints are its poster's
    // queued work, which an ordinary restart must keep.
    let mut last: Option<(u64, String, AccountId)> = None;
    let scan = crate::log::SegmentedLog::open(dir, |entry| {
        if let crate::log::LogEntry::Checkpoint(checkpoint) = &entry {
            last = Some((checkpoint.seq, checkpoint.root.clone(), checkpoint.signer.parse()?));
        }
        Ok(())
    });
    let last = match scan {
        Ok(_) => last,
        Err(e) => {
            let repaired = repair(&e);
            // Retry once after the repair.
            crate::log::SegmentedLog::open(dir, |entry| {
                if let crate::log::LogEntry::Checkpoint(checkpoint) = &entry {
                    last = Some((checkpoint.seq, checkpoint.root.clone(), checkpoint.signer.parse()?));
                }
                Ok(())
            })?;
            if last.is_none() {
                return Err(repaired);
            }
            last
        }
    };
    let last = last.as_ref();
    if last.is_some_and(|(_, _, signer)| *signer == state.signer) {
        return Ok(());
    }
    if last.is_some_and(|(seq, root, _)| *seq > anchor_seq || *root != anchor_root) {
        std::fs::write(
            &marker,
            serde_json::to_string(&crate::checkpoints::Takeover {
                seq: anchor_seq,
                root: anchor_root.clone(),
                signer: state.signer,
            })
            .context("encoding the takeover marker")?,
        )
        .context("writing the takeover marker")?;
        crate::log::truncate_to_checkpoint(dir, anchor_seq, &anchor_root).map_err(|e| repair(&e))?;
        std::fs::remove_file(&marker).context("removing the takeover marker")?;
    }
    Ok(())
}

/// How often a running node with sealed keys reads the vault's current release.
pub(super) const RELEASE_CHECK_EVERY: Duration = Duration::from_secs(60);

/// Raises `release_superseded` (Critical) once the vault's current release at finality is no longer
/// `sealed` in `generation`, the release and generation the keys are sealed in, reading it every
/// `every`: a restart would then find no keys to open, so they must be handed to the release the
/// vault names before this node stops. A failed read is retried.
/// Error cases:
/// - none; it runs until the node stops
pub(super) async fn watch_release(
    vault: impl CheckpointChain,
    sealed: Release,
    generation: u64,
    every: Duration,
) -> anyhow::Result<()> {
    let mut superseded = false;
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        let current = match current_release(&vault).await {
            Ok(current) => current,
            Err(e) => {
                warn!("reading the vault's current release failed: {e:#}");
                continue;
            }
        };
        let named = current.id == sealed.id() && current.generation == generation;
        if !named && !superseded {
            error!(
                alert = "release_superseded",
                "the vault now names release 0x{} in generation {}, not this one's keys (release 0x{}, generation \
                 {generation}), so a restart finds no keys to open: hand them to the release it names before this \
                 node stops",
                hex::encode(current.id),
                current.generation,
                hex::encode(sealed.id())
            );
        } else if named && superseded {
            info!(alert_resolved = "release_superseded", "the vault names this release again");
        }
        superseded = !named;
    }
}

/// Refuses the first start after a handover unless the log ends at the predecessor's final
/// checkpoint, so this release never continues from a log the host truncated, replaced, or extended
/// at the handover; once it passes, the head is forgotten, since this release then extends the log.
/// Error cases:
/// - the log does not end at that checkpoint, or forgetting the head cannot be sealed
pub(super) fn check_predecessor_head(
    store: Option<&KeyStore>,
    last_seq: Option<u64>,
    last_checkpoint: Option<&Checkpoint>,
) -> anyhow::Result<()> {
    let Some(head) = store.and_then(|store| store.keys().predecessor_head) else { return Ok(()) };
    let ends_at_head =
        last_seq == Some(head.seq) && last_checkpoint.is_some_and(|cp| cp.seq == head.seq && cp.root == head.root);
    ensure!(
        ends_at_head,
        "the log does not end at the predecessor's final checkpoint (seq {}, root {}): restore the \
         predecessor's final log and start again",
        head.seq,
        head.root
    );
    let store = store.expect("a head comes from a store");
    store.update(|keys| keys.predecessor_head = None).context("forgetting the predecessor's checkpoint")
}

/// Why a release with no keys sealed in `dir` for the vault's `generation` of it, and no flag, does
/// not start.
fn refusal(dir: &Path, generation: u64) -> String {
    format!(
        "no keys are sealed for this release in {} in the vault's generation {generation} of it: start with \
         --predecessor to take them from the release running now, as a release governance names again does, \
         or with --new-key to seal new ones, which the vault must register",
        dir.display()
    )
}

#[cfg(test)]
mod tests;
