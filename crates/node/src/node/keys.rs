//! The enclave's keys for this start, and their attestation, which real attestation gives only to a
//! guest whose kernel times on the TSC Secure TSC protects. With `[tee.sealing]` they are the keys
//! sealed for this release, new ones only on the operator's `--new-key`, or the previous release's,
//! taken over with `--predecessor`; with none of these the node refuses to start. With
//! `[checkpoints]`, the vault's current release must be this one before it opens or seals keys,
//! which it does in the vault's generation of the release, and the predecessor checks the same
//! before it hands them over; a release governance names again takes them afresh. A mock
//! configuration without sealing uses a fresh key, or the published one, for this process only, and
//! one with sealing but without `[checkpoints]` reads no vault and seals in generation 0.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use tracing::{error, info, warn};

use crate::checkpoints::{CheckpointChain, CurrentRelease};
use crate::config::{NodeConfig, TeeProvider};
use crate::handover::{current_release, ensure_named, vault_reader};
use crate::log::Checkpoint;
use crate::log_check::VerifiedHead;
use crate::metrics::Metrics;
use crate::tee::certificates::CertificateStore;
use crate::tee::clock::GuestClock;
use crate::tee::keys::{Head, KeySet};
use crate::tee::sealing::{KeyStore, Release, file_for};
use crate::tee::{Attestation, AttestationBundle, Attestor, EnclaveKey, attest_bundle};
use crate::tls::TlsIdentity;
use engine_core::turns::TURN_MS;
use engine_core::types::Millis;

mod platform;
pub(super) use platform::{attestor_for, sealer};
mod takeover;
pub(super) use takeover::cut_to_finalized_anchor;

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
    /// Admit no client request (`run --frozen`): a log's last start before a new log carries its state.
    pub frozen: bool,
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
/// - with real attestation, the guest kernel does not time on the TSC in the frequency the
///   hardware reports (alert `clocksource_unprotected`)
/// - the keys cannot be obtained (see `obtain`), or the attestor cannot produce a quote
pub(super) async fn attested(
    cfg: &NodeConfig,
    start: &KeyStart,
    metrics: &Metrics,
    floor: Option<Millis>,
) -> anyhow::Result<Keys> {
    if cfg.tee.provider == TeeProvider::Mock {
        warn!("mock TEE: attestation reports are not backed by hardware");
    }
    let attestor = attestor_for(cfg.tee.provider, &GuestClock::default())
        .inspect_err(|e| error!(alert = "clocksource_unprotected", "refusing to serve: {e:#}"))?;
    let (enclave, tls, store) = obtain(cfg, start, attestor.as_ref(), metrics, floor)
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
/// names, if any, at trusted time never below `floor`, which a member's last handover reached (see
/// `obtain_with`).
/// Error cases:
/// - the vault's client cannot be built, or see `obtain_with`
async fn obtain(
    cfg: &NodeConfig,
    start: &KeyStart,
    attestor: &dyn Attestor,
    metrics: &Metrics,
    floor: Option<Millis>,
) -> anyhow::Result<(EnclaveKey, TlsIdentity, Option<KeyStore>)> {
    let floor = floor.unwrap_or(Millis::MIN);
    let sampled = async { crate::handover::turns::sampled_now(cfg).await.map(|reached| reached.max(floor)) };
    let assignments = crate::handover::assigned::for_config(cfg, "start_assignments")?;
    obtain_with(cfg, start, attestor, (vault_reader(cfg)?.as_ref(), assignments), sampled, metrics).await
}

/// `obtain`, reading the vault through `vault`, when the config names one, the turn's latest assignment
/// through `assignments`, with `[log_heads]` (#596), and the time trusted time has reached through
/// `reached`, which it awaits only while rotation is on and a daily turn is drawn.
/// Error cases:
/// - a flag is given without `[tee.sealing]`, or real attestation runs without it
/// - the stand-in measurement is missing under the mock TEE, or given under real attestation
/// - the vault cannot be read, or, without `--predecessor` or an election naming this release,
///   its current release is not this one
/// - the sealed keys cannot be opened, or a flag is given beside them
/// - the release has no keys sealed in the vault's generation of it and no flag: refused while
///   rotation is off or the registry names no handover source for the incumbent; new keys cannot
///   be sealed, the predecessor's log cannot be fetched, or the handover fails
async fn obtain_with(
    cfg: &NodeConfig,
    start: &KeyStart,
    attestor: &dyn Attestor,
    (vault, assignments): (Option<&impl CheckpointChain>, Option<crate::handover::assigned::Reader>),
    reached: impl Future<Output = anyhow::Result<Millis>>,
    metrics: &Metrics,
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
                "the vault's release change did not fold into its generation at finality within a minute; call syncRelease \
                 (anyone may) and start again once its block is finalized",
            )??;
            Some(folded)
        }
        vault => match vault {
            Some(vault) => Some(current_release(vault).await?),
            None => None,
        },
    };
    // From a daily turn's boundary the vault names its member's release before finality shows it, as the turn
    // the finalised schedule names at trusted time does, or a skip of it naming this release (#596).
    let turn = match vault {
        Some(vault) if cfg.operator.rotation_enabled => crate::handover::turns::at_start(vault, reached).await?,
        _ => None,
    };
    let turn = crate::handover::assigned::own_skip(assignments.as_deref(), turn, &release).await;
    if let Some(turn) = &turn {
        crate::handover::turns::ensure_turn_names(turn, &release)
            .context("refusing this release's keys: a daily turn names another release")?;
    }
    let generation = turn.as_ref().map(|turn| turn.generation).or(current.map(|current| current.generation));
    let generation = generation.unwrap_or(0);
    let sealed = file_for(dir, &release, generation).exists();
    ensure!(
        !sealed || (!start.new_key && start.predecessor.is_none()),
        "keys are already sealed for this release in {}; start without --new-key or --predecessor",
        dir.display()
    );
    // A predecessor checks this itself before it hands its keys over.
    if let (Some(current), None, None) = (current, &start.predecessor, &turn) {
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
        // Trusted time has reached a turn's boundary when the schedule names this release for it.
        let reached = turn.as_ref().map(|turn| turn.boundary);
        let parts = (sealer.as_ref(), attestor);
        crate::handover::take_with(cfg, address, parts, vault, reached, metrics, assignments).await?
    } else if let (Some(vault), Some(turn)) = (vault, &turn) {
        // No sealed keys and no flag, and the schedule names this release for the turn trusted time
        // has reached: take the keys from the operator the turn takes over from, or, when that one
        // never took its own turn, from the one before it.
        let at = vault.observe(crate::checkpoints::View::Finalized, &[]).await?;
        crate::node::member::before_whole_log_take(cfg, start)?;
        let parts = (sealer.as_ref(), attestor);
        crate::handover::turns::take_keys(cfg, vault, (&at, turn), parts, metrics, assignments).await?
    } else if let Some(vault) = vault {
        // No sealed keys and no flag: while the registry has rotation on and the vault names this
        // release — so an election has taken effect naming this node the operator — take the keys
        // from the incumbent the election replaced, reading its handover endpoint from the
        // registry, and the published log ahead of the segment the handover itself carries.
        info!("election handover: no sealed keys; checking the registry's rotation");
        ensure!(vault.rotation_enabled().await?, "{}", refusal(dir, generation));
        info!("election handover: rotation is on; resolving the incumbent the election replaced");
        match crate::handover::resolve_source(vault).await? {
            Some(source) => {
                info!(predecessor = %source.endpoint, "election handover: resolved the incumbent");
                let at = vault.observe(crate::checkpoints::View::Finalized, &[]).await?;
                let signers = crate::handover::vault_signers(vault, &at).await?;
                crate::node::member::before_whole_log_take(cfg, start)?;
                let snapshots = cfg.daily_snapshots.as_ref().map(|daily| daily.dir.as_path());
                let finalized = crate::handover::Finalized::read(vault, &at, snapshots).await;
                crate::handover::sync_predecessor_log(cfg, &source, (signers, finalized.as_ref()), metrics).await?;
                info!("election handover: fetched the incumbent's published log");
                info!(
                    predecessor = %source.endpoint,
                    incumbent = %source.candidate,
                    incumbent_release = %hex::encode(source.release),
                    "the vault names this release through an election in effect: taking the keys from the \
                     operator it replaced"
                );
                let parts = (sealer.as_ref(), attestor);
                crate::handover::take_with(cfg, &source.endpoint, parts, vault, None, metrics, assignments).await?
            }
            None => bail!("{}", refusal(dir, generation)),
        }
    } else {
        bail!(refusal(dir, generation))
    };
    let keys = store.keys();
    Ok((keys.enclave_key()?, TlsIdentity::new(keys.tls_key()?)?, Some(store)))
}

/// This release's sealed certificate store (`tee::certificates`), with `[tee.sealing]` and
/// `[api.tls.acme]`. A start opens it before the keys, so one that does not open refuses before a
/// handover takes them from the operator in effect.
/// Error cases:
/// - the platform's sealing is not available (alert `enclave_key_failed`, see `sealer`)
/// - the store does not open (alert `sealed_certificates_unreadable`, see `CertificateStore::open`)
pub(super) fn certificates(cfg: &NodeConfig, start: &KeyStart) -> anyhow::Result<Option<Arc<CertificateStore>>> {
    let (Some(sealing), Some(_)) = (&cfg.tee.sealing, &cfg.api.tls) else { return Ok(None) };
    let sealer = sealer(cfg.tee.provider, start)
        .inspect_err(|e| error!(alert = "enclave_key_failed", "refusing to serve: {e:#}"))?;
    let store = CertificateStore::open(&sealing.dir, sealer, cfg.file_sha256)
        .inspect_err(|e| error!(alert = "sealed_certificates_unreadable", "refusing to serve: {e:#}"))?;
    Ok(Some(Arc::new(store)))
}

/// How often a running node with sealed keys reads the vault's current release.
pub(super) const RELEASE_CHECK_EVERY: Duration = Duration::from_secs(60);

/// How long from a daily turn's boundary, by trusted time, a release the turn does not name waits for
/// the turn's member to take its keys before `release_superseded` pages, which a routine handover
/// finishes well within: fifteen minutes, or half a turn when that is shorter.
pub(super) const TURN_HANDOVER_GRACE_MS: Millis = if TURN_MS / 2 < 900_000 { TURN_MS / 2 } else { 900_000 };

/// The release the vault names at custody finality, whether or not its generation has folded the
/// change: unlike `handover::current_release`, this read must not wait for the fold, since the
/// supersession watch alerts on the release move itself.
/// Error cases:
/// - the vault cannot be read
async fn named_release(vault: &impl CheckpointChain) -> anyhow::Result<CurrentRelease> {
    let finalized = vault.observe(crate::checkpoints::View::Finalized, &[]).await?;
    vault.current_release(&finalized).await
}

/// Raises `release_superseded` (Critical) once the vault's current release at finality, read every `every`,
/// is no longer the release and generation `store`'s keys are sealed in: a restart would find no keys to open,
/// so they must go to the release named before this node stops, superseded from the block the change reads
/// at finality, not from the vault's fold of it; a failed read is retried. From a turn's boundary by `trusted`
/// time, the finalised schedule's turn names it (`handover::turns`), or the latest agreed block's skip (#596);
/// a turn naming another holding pages only `TURN_HANDOVER_GRACE_MS` past its boundary, as its member takes
/// the keys or this node seals them in the turn's generation (`sequencer::turns`).
/// Error cases:
/// - none; it runs until the node stops
pub(super) async fn watch_release(
    vault: impl CheckpointChain,
    store: Arc<KeyStore>,
    every: Duration,
    trusted: crate::trusted_time::TrustedClock,
    assignments: Option<Arc<dyn crate::handover::assigned::Assignments>>,
) -> anyhow::Result<()> {
    let (mut superseded, mut waiting) = (false, false);
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        let (sealed, generation) = (store.release(), store.generation());
        let current = match named_release(&vault).await {
            Ok(current) => current,
            Err(e) => {
                warn!("reading the vault's current release failed: {e:#}");
                continue;
            }
        };
        let reached = trusted.reached_ms();
        // Before trusted time a drawn schedule's turn cannot be judged, and the vault's release alone
        // still names the drawn member's after a promotion moved the turn on: it waits for a sample.
        if reached.is_none() && crate::handover::turns::finalised_drawn(&vault).await {
            continue;
        }
        let (named, handing_over) = match crate::handover::turns::names(&vault, &sealed, generation, reached).await {
            Some((false, boundary)) => {
                let latest = crate::handover::assigned::names_holding(assignments.as_deref(), sealed.id(), generation);
                let Some(latest) = latest.await else { continue };
                (latest, reached.is_some_and(|reached| reached < boundary.saturating_add(TURN_HANDOVER_GRACE_MS)))
            }
            Some((true, _)) => (true, false),
            None => (current.id == sealed.id() && current.generation == generation, false),
        };
        if !named && handing_over {
            if !waiting {
                info!(
                    "a daily turn names another holding from its boundary: waiting for its member to take the keys, or \
                     for this node to seal them in the turn's generation as it takes the turn"
                );
            }
            waiting = true;
            continue;
        }
        waiting = false;
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
/// at the handover; once it passes, the head is forgotten, since this release then extends the log,
/// and the head the boundary's log check verified becomes what `check` makes of the one the keys
/// carried and the handed-over checkpoint, sealed in the same write (`log_check::own_check`).
/// Error cases:
/// - the log does not end at that checkpoint, or forgetting the head cannot be sealed
pub(crate) fn check_predecessor_head(
    store: Option<&KeyStore>,
    last_seq: Option<u64>,
    last_checkpoint: Option<&Checkpoint>,
    check: impl FnOnce(Option<VerifiedHead>, &[crate::log_check::Holder], &Head) -> Option<VerifiedHead>,
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
    let update = |keys: &mut KeySet| {
        let checked = check(keys.log_check, &keys.holders, &head);
        crate::log_check::seal_verified(keys, checked);
        keys.predecessor_head = None;
    };
    store.update(update).context("forgetting the predecessor's checkpoint")
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
