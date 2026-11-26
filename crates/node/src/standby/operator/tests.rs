//! Which operator a follower follows, and which signers a promoted standby's copy replays against:
//! the registry's and the vault's under rotation, `[standby]`'s without it.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::bail;
use engine_core::Fixed;

use engine_core::command::Command;

use super::*;
use crate::checkpoints::{Takeover, VaultState};
use crate::log::{Checkpoint, LogEntry};
use crate::test_support::{TestSequencer, dev_config, log_entries, scratch_dir};

/// Under rotation the follower follows the candidate the registry names in effect, at its
/// registered hostname, against the vault's signer; when the turn changes, so does the origin.
#[tokio::test]
async fn a_turn_change_moves_the_follower_to_the_new_operators_hostname() {
    let mut cfg = dev_config();
    cfg.operator.rotation_enabled = true;
    cfg.standby = Some(standby());
    let registry = Registry { hostname: Mutex::new("a.operator.example".into()), signer: AccountId([0x51; 20]) };
    let followed = resolve(&cfg, &registry).await.unwrap();
    assert_eq!(followed.origin, "https://a.operator.example");
    assert_eq!(followed.enclave, AccountId([0x51; 20]), "frames verify against the vault's signer");

    *registry.hostname.lock().unwrap() = "b.operator.example".into();
    let followed = resolve(&cfg, &registry).await.unwrap();
    assert_eq!(followed.origin, "https://b.operator.example", "the follower stayed on the last turn's operator");

    *registry.hostname.lock().unwrap() = String::new();
    assert!(resolve(&cfg, &registry).await.is_err(), "an unregistered hostname was followed");
}

/// With rotation off, the follower follows the operator `[standby]` names, whatever the registry
/// says, and needs the section.
#[tokio::test]
async fn with_rotation_off_the_follower_keeps_the_operator_standby_names() {
    let mut cfg = dev_config();
    cfg.operator.rotation_enabled = false;
    cfg.standby = Some(standby());
    let registry = Registry { hostname: Mutex::new("a.operator.example".into()), signer: AccountId([0x51; 20]) };
    assert_eq!(resolve(&cfg, &registry).await.unwrap(), Followed::from_standby(&standby()));

    cfg.standby = None;
    let err = resolve(&cfg, &registry).await.unwrap_err();
    assert!(format!("{err:#}").contains("[standby]"), "{err:#}");
}

/// A follower reads the operator's archive from the bucket its own `[archive]` names first, under the log's chain and
/// vault, with rotation on or off, and from no bucket without the section.
#[tokio::test]
async fn a_follower_reads_the_bucket_its_own_archive_section_names_first() {
    let mut cfg = dev_config();
    cfg.standby = Some(standby());
    let registry = Registry { hostname: Mutex::new("a.operator.example".into()), signer: AccountId([0x51; 20]) };
    assert_eq!(resolve(&cfg, &registry).await.unwrap().bucket, None);
    let public: reqwest::Url = "https://archive.example".parse().unwrap();
    cfg.archive = Some(crate::config::ArchiveSection {
        endpoint: "https://account.r2.example".parse().unwrap(),
        bucket: "engine-log".into(),
        region: "auto".into(),
        public_url: public.clone(),
        access_key_id_secret: "archive_key_id".to_owned().try_into().unwrap(),
        secret_access_key_secret: "archive_secret_key".to_owned().try_into().unwrap(),
        lag_alert: Duration::from_secs(3600),
    });
    let prefix = crate::archive::log_prefix(cfg.custody.chain_id, cfg.custody.vault.parse().unwrap());
    let bucket = crate::archive::Bucket::new(&public, &prefix).unwrap();
    for rotation in [false, true] {
        cfg.operator.rotation_enabled = rotation;
        let followed = resolve(&cfg, &registry).await.unwrap();
        assert_eq!(followed.bucket.as_ref(), Some(&bucket), "rotation {rotation}");
        assert_eq!(
            followed.archive(),
            crate::archive::Origins { bucket: Some(bucket.clone()), nodes: followed.origins() }
        );
    }
}

/// A `--standby` node with rotation on follows the registry's operator in effect, read again as the
/// turn moves, with `[standby]`'s archive mirrors and certificate setting; with rotation off, the
/// operator `[standby]` names.
#[tokio::test]
async fn a_standby_with_rotation_on_follows_the_registrys_operator_with_its_own_mirrors() {
    let mut cfg = dev_config();
    cfg.operator.rotation_enabled = true;
    let section = StandbySection { danger_accept_invalid_certs: true, ..standby() };
    let registry = Registry { hostname: Mutex::new("a.operator.example".into()), signer: AccountId([0x51; 20]) };
    let followed = for_standby(&cfg, &section, &registry).await.unwrap();
    assert_eq!((followed.origin.as_str(), followed.enclave), ("https://a.operator.example", AccountId([0x51; 20])));
    assert_eq!((followed.mirrors, followed.danger_accept_invalid_certs), (section.mirrors.clone(), true));
    *registry.hostname.lock().unwrap() = "b.operator.example".into();
    let moved = for_standby(&cfg, &section, &registry).await.unwrap();
    assert_eq!(moved.origin, "https://b.operator.example", "the standby stayed on the last turn's operator");

    cfg.operator.rotation_enabled = false;
    cfg.standby = Some(section.clone());
    assert_eq!(for_standby(&cfg, &section, &registry).await.unwrap(), Followed::from_standby(&section));
}

/// A standby follows another operator when the origin moves, or when the vault's signer is replaced
/// at the same hostname: frames then verify against the new enclave.
#[tokio::test]
async fn a_signer_replaced_at_the_same_hostname_moves_the_standby() {
    let mut cfg = dev_config();
    cfg.operator.rotation_enabled = true;
    let section = standby();
    let registry = Registry { hostname: Mutex::new("a.operator.example".into()), signer: AccountId([0x51; 20]) };
    let before = for_standby(&cfg, &section, &registry).await.unwrap();
    assert!(!for_standby(&cfg, &section, &registry).await.unwrap().moved_from(&before), "nothing changed");
    let replaced = Registry { signer: AccountId([0x52; 20]), ..registry };
    let after = for_standby(&cfg, &section, &replaced).await.unwrap();
    assert_eq!((after.origin.as_str(), after.enclave), ("https://a.operator.example", AccountId([0x52; 20])));
    assert!(after.moved_from(&before), "a setSigner replacement at the same hostname was not followed");
}

/// A takeover finalizes a copy the vault's signer signed, though `[standby]` names another enclave,
/// whatever the rotation switch: it replays against the signers the vault recorded and the followed
/// enclave, as the cold start does. Against a set that names neither, the copy is refused.
#[tokio::test]
async fn a_takeover_replays_the_copy_against_the_vaults_signers_whatever_the_switch() {
    let signer = AccountId(crate::tee::EnclaveKey::test_key().address());
    let dir = scratch_dir("standby-promotion-signers");
    let node = TestSequencer::start(&dir, 1024);
    for _ in 0..6 {
        node.submit(Command::Tick).await;
    }
    node.stop().await;
    let Some(LogEntry::Checkpoint(last)) = log_entries(&dir).pop() else { panic!("the log ends at a checkpoint") };
    let takeover = Takeover { seq: last.seq, root: last.root.clone(), signer };
    let (mut cfg, section) = (dev_config(), standby());
    let registry = Registry { hostname: Mutex::new("a.operator.example".into()), signer };
    let (archive, domain) = (super::super::archive::archive_client(false).unwrap(), DOMAIN);
    // No archive answers: the copy is what the stream left.
    let origins = ["http://127.0.0.1:9".to_owned()];
    let finalize = async |dir: &std::path::Path, signers| {
        let copied = super::super::chain::Chain::new(domain, signer);
        let copy = (dir, None);
        super::super::finalize(
            &archive,
            (domain, signers, Default::default()),
            &origins,
            copy,
            (&takeover, Default::default()),
            copied,
        )
        .await
    };
    let err = finalize(&dir, BTreeSet::from([section.enclave])).await.expect_err("another enclave's set took the copy");
    assert!(format!("{err:#}").contains("refusing to truncate the copy"), "{err:#}");
    for rotation in [true, false] {
        cfg.operator.rotation_enabled = rotation;
        let signers = copy_signers(&cfg, &registry, &Followed::from_standby(&section)).await.unwrap();
        assert_eq!(signers, BTreeSet::from([signer, section.enclave]), "rotation {rotation}");
        finalize(&dir, signers).await.unwrap_or_else(|e| panic!("rotation {rotation}: the copy was refused: {e:#}"));
    }
}

/// A takeover's signer read that fails gives no signers and pages `standby_signal_failed` once while it
/// keeps failing, and a read that works ends the episode; the first read after a start ends one a
/// restart's alert outbox kept open.
#[tokio::test]
async fn a_takeover_signer_read_pages_once_per_episode_and_ends_it_once_it_works() {
    let (_guard, mut alerts, mut resolved) = crate::test_support::capture_alerts_and_resolutions();
    let (cfg, followed) = (dev_config(), Followed::from_standby(&standby()));
    let working = Epochs { epochs: 3, failing: None };
    let mut signal_failed = None;
    assert!(takeover_signers(&cfg, &working, &followed, &mut signal_failed).await.is_some());
    let ended = std::iter::from_fn(|| resolved.try_recv().ok()).count();
    assert_eq!(ended, 1, "a start's first read that works did not end a kept episode");
    for _ in 0..2 {
        let read = takeover_signers(&cfg, &Epochs { epochs: 3, failing: Some(2) }, &followed, &mut signal_failed);
        assert!(read.await.is_none(), "a failed read gave signers");
    }
    let paged = std::iter::from_fn(|| alerts.try_recv().ok()).filter(|alert| alert["alert"] == "standby_signal_failed");
    assert_eq!(paged.count(), 1, "a failing read paged other than once");
    let read = takeover_signers(&cfg, &working, &followed, &mut signal_failed).await;
    assert!(read.is_some() && signal_failed == Some(false), "a read that works gave no signers");
    let mut ended = std::iter::from_fn(|| resolved.try_recv().ok());
    assert!(ended.any(|line| line["alert_resolved"] == "standby_signal_failed"), "the episode did not end");
}

/// A copy's signers are the vault's at finality and the followed operator's enclave, which the vault names
/// at the latest block before finality does, whatever the rotation switch; every recorded epoch is read,
/// and one whose read fails fails the set rather than leaving its signer out.
#[tokio::test]
async fn a_copys_signers_are_every_recorded_epochs_and_the_followed_enclave() {
    let (mut cfg, section) = (dev_config(), standby());
    let registry = Registry { hostname: Mutex::new("a.operator.example".into()), signer: AccountId([0x51; 20]) };
    for rotation in [true, false] {
        cfg.operator.rotation_enabled = rotation;
        let followed = Followed { enclave: AccountId([0x52; 20]), ..Followed::from_standby(&section) };
        let signers = copy_signers(&cfg, &registry, &followed).await.unwrap();
        assert_eq!(signers, BTreeSet::from([AccountId([0x51; 20]), AccountId([0x52; 20])]), "rotation {rotation}");
    }
    let followed = Followed::from_standby(&section);
    let signers = copy_signers(&cfg, &Epochs { epochs: 300, failing: None }, &followed).await.unwrap();
    assert_eq!(signers, (1..=300).map(signer_of).chain([section.enclave]).collect(), "a recorded epoch is missing");
    let failed = copy_signers(&cfg, &Epochs { epochs: 3, failing: Some(2) }, &followed).await.unwrap_err();
    assert!(format!("{failed:#}").contains("epoch 2"), "{failed:#}");
}

// ── Helpers ──

/// The signer the `Epochs` vault records for `epoch`.
fn signer_of(epoch: u64) -> AccountId {
    let mut address = [0u8; 20];
    address[12..].copy_from_slice(&epoch.to_be_bytes());
    AccountId(address)
}

/// A vault that has recorded `epochs` signer epochs, `signer_of` each, the last its current signer,
/// whose read of epoch `failing` fails.
struct Epochs {
    epochs: u64,
    failing: Option<u64>,
}

impl CheckpointChain for Epochs {
    async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
        Ok(VaultState {
            height: 1,
            signer: signer_of(self.epochs),
            checkpoint: None,
            balance: Fixed::ZERO,
            deposits: 0,
            deposited: Fixed::ZERO,
            distributions_paid: Fixed::ZERO,
        })
    }

    async fn observe(
        &self,
        view: View,
        _approvals: &[crate::checkpoints::ApprovalId],
    ) -> anyhow::Result<crate::checkpoints::Observation> {
        let state = self.state(view).await?;
        let lifecycle = crate::checkpoints::LifecycleEvidence {
            height: 1,
            block_hash: engine_core::eip712::B256::repeat_byte(1),
            signer: state.signer,
            signer_epoch: self.epochs,
            withdrawals_paused: false,
            total_withdrawn: Fixed::ZERO,
            checkpoint_seq: None,
            checkpoint_root: None,
            approvals: Vec::new(),
        };
        Ok(crate::checkpoints::Observation { view, state, lifecycle: Some(lifecycle) })
    }

    async fn post(&self, _checkpoint: &Checkpoint) -> anyhow::Result<()> {
        bail!("this vault takes no checkpoints")
    }

    async fn epoch_signers(
        &self,
        _at: &crate::checkpoints::Observation,
        epochs: &[u64],
    ) -> anyhow::Result<Vec<AccountId>> {
        epochs
            .iter()
            .map(|epoch| match self.failing == Some(*epoch) {
                true => Err(anyhow::anyhow!("the read of epoch {epoch} timed out")),
                false => Ok(signer_of(*epoch)),
            })
            .collect()
    }
}

/// The deployment the test sequencer seals and signs for.
const DOMAIN: crate::log::CheckpointDomain =
    crate::log::CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// A registry naming `hostname` for the operator in effect, behind a vault whose signer is `signer`.
struct Registry {
    hostname: Mutex<String>,
    signer: AccountId,
}

impl CheckpointChain for Registry {
    async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
        Ok(VaultState {
            height: 1,
            signer: self.signer,
            checkpoint: None,
            balance: Fixed::ZERO,
            deposits: 0,
            deposited: Fixed::ZERO,
            distributions_paid: Fixed::ZERO,
        })
    }

    async fn post(&self, _checkpoint: &Checkpoint) -> anyhow::Result<()> {
        bail!("this vault takes no checkpoints")
    }

    async fn operator_hostname(&self) -> anyhow::Result<String> {
        Ok(self.hostname.lock().unwrap().clone())
    }
}

/// A `[standby]` section naming another operator than the registry.
fn standby() -> StandbySection {
    StandbySection {
        operator: "https://standby.operator.example".into(),
        mirrors: vec!["https://mirror.example".into()],
        enclave: AccountId([0x77; 20]),
        poll_interval: Duration::from_secs(30),
        danger_accept_invalid_certs: false,
    }
}
