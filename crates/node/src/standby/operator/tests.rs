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

/// A promotion under rotation finalizes a copy the vault's signer signed, though `[standby]` names
/// another enclave: the copy replays against the signers the vault recorded. Against the configured
/// enclave alone it would not, which is what rotation off keeps.
#[tokio::test]
async fn a_promotion_replays_the_copy_against_the_vaults_signers_under_rotation() {
    let signer = AccountId(crate::tee::EnclaveKey::test_key().address());
    let (dir, refused) = (scratch_dir("standby-promotion-signers"), scratch_dir("standby-promotion-configured"));
    let node = TestSequencer::start(&dir, 1024);
    for _ in 0..6 {
        node.submit(Command::Tick).await;
    }
    node.stop().await;
    std::fs::create_dir_all(&refused).unwrap();
    for file in std::fs::read_dir(&dir).unwrap().map(Result::unwrap) {
        std::fs::copy(file.path(), refused.join(file.file_name())).unwrap();
    }
    let Some(LogEntry::Checkpoint(last)) = log_entries(&dir).pop() else { panic!("the log ends at a checkpoint") };
    let takeover = Takeover { seq: last.seq, root: last.root.clone(), signer };
    let (mut cfg, section) = (dev_config(), standby());
    cfg.operator.rotation_enabled = true;
    let registry = Registry { hostname: Mutex::new("a.operator.example".into()), signer };
    let signers = takeover_signers(&cfg, &section, &registry).await.unwrap();
    assert_eq!(signers, BTreeSet::from([signer]), "the takeover replays against another set than the vault's");
    let (archive, domain) = (super::super::archive::archive_client(false).unwrap(), DOMAIN);
    // No archive answers: the copy is what the stream left.
    let origins = ["http://127.0.0.1:9".to_owned()];
    let finalize = async |dir: &std::path::Path, signers| {
        let copied = super::super::chain::Chain::new(domain, signer);
        super::super::finalize(&archive, signers, &origins, dir, &takeover, copied).await
    };
    finalize(&dir, signers).await.expect("a copy the vault's signer signed was refused");

    cfg.operator.rotation_enabled = false;
    let configured = takeover_signers(&cfg, &section, &registry).await.unwrap();
    assert_eq!(configured, BTreeSet::from([section.enclave]), "rotation off replays against another enclave");
    let err = finalize(&refused, configured).await.expect_err("a copy another enclave signed was finalized");
    assert!(format!("{err:#}").contains("does not replay"), "{err:#}");
}

// ── Helpers ──

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
