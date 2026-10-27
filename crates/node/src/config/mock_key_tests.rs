//! Historical V19 withdrawal/storage fixtures; current envelopes are covered by access tests.
//! Published mock authority is explicit, stable, and unavailable to hardware attestation.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, watch};

use crate::config::{AuthMode, NodeConfig, TeeProvider, TeeSection, public_key_address};
use crate::log::{LogEntry, Manifest};
use crate::sequencer::{Request, Sequencer};
use crate::tee::EnclaveKey;
use crate::test_support::{
    DEV_CONFIG, deposit, dev_config, log_entries, own_operator_key, scratch_dir, sequencer_config, trusted_now,
    withdraw,
};

#[test]
fn public_mock_authority_is_opt_in_and_real_attestation_refuses_it_even_without_validation() {
    let default: TeeSection = toml::from_str("provider='mock'").unwrap();
    assert!(!default.use_published_mock_key);
    assert_ne!(
        EnclaveKey::for_config(&default).unwrap().address(),
        EnclaveKey::for_config(&default).unwrap().address()
    );
    let text = std::fs::read_to_string(DEV_CONFIG).unwrap();
    let mut cfg: NodeConfig = toml::from_str(&format!("{text}\n[alerts.critical]\nurl_env='PAGER'\n")).unwrap();
    let first = EnclaveKey::for_config(&cfg.tee).unwrap();
    let second = EnclaveKey::for_config(&cfg.tee).unwrap();
    assert_eq!(first.address(), second.address());
    assert_eq!(first.address(), EnclaveKey::test_key().address());
    cfg.tee.provider = TeeProvider::ConfigfsTsm;
    cfg.node.auth = AuthMode::Eip712;
    cfg.operator.public_key = own_operator_key();
    assert!(cfg.validate().unwrap_err().to_string().contains("use_published_mock_key"));
    assert!(EnclaveKey::for_config(&cfg.tee).err().unwrap().to_string().contains("use_published_mock_key"));
    cfg.tee.use_published_mock_key = false;
    cfg.validate().unwrap();
}

#[test]
fn public_mock_signer_is_in_the_compiled_published_key_policy() {
    let key = EnclaveKey::for_config(&dev_config().tee).unwrap();
    let keys: toml::Value = toml::from_str(include_str!("../../../../config/test-keys.toml")).unwrap();
    assert!(
        keys["operator_public_keys"].as_array().unwrap().iter().any(|key_text| public_key_address(
            key_text.as_str().unwrap()
        )
        .unwrap()
        .0 == key.address())
    );
}

#[tokio::test]
async fn configured_mock_authority_recovers_completed_withdrawal_history_without_resigning() {
    let cfg = dev_config();
    let dir = scratch_dir("configured-mock-key-restart");
    let key = Arc::new(EnclaveKey::for_config(&cfg.tee).unwrap());
    let (signed, mut signed_rx) = mpsc::unbounded_channel();
    let mut seq = Sequencer::recover(
        sequencer_config(&dir, watch::channel(Manifest::default()).0),
        key,
        broadcast::channel(128).0,
        signed.clone(),
    )
    .unwrap();
    seq.activate_legacy_protocol().unwrap();
    seq.genesis(&cfg.genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(seq.run(rx));
    for cmd in [deposit(100, "configured-mock-restart"), withdraw(40)] {
        let (reply, done) = tokio::sync::oneshot::channel();
        tx.send(Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
        done.await.unwrap();
    }
    drop(tx);
    task.await.unwrap().unwrap();
    assert_eq!(signed_rx.try_recv().unwrap().amount, engine_core::Fixed::from_int(40));
    let before = log_entries(&dir);
    assert!(before.iter().any(|entry| matches!(entry, LogEntry::Withdrawal(_))));
    let recovered = Sequencer::recover(
        sequencer_config(&dir, watch::channel(Manifest::default()).0),
        Arc::new(EnclaveKey::for_config(&cfg.tee).unwrap()),
        broadcast::channel(128).0,
        signed.clone(),
    )
    .unwrap();
    drop(recovered);
    assert!(signed_rx.try_recv().is_err(), "completed history never signs twice");
    assert_eq!(serde_json::to_value(log_entries(&dir)).unwrap(), serde_json::to_value(&before).unwrap());
    let wrong = Sequencer::recover(
        sequencer_config(&dir, watch::channel(Manifest::default()).0),
        Arc::new(EnclaveKey::generate()),
        broadcast::channel(128).0,
        signed,
    );
    assert!(wrong.err().unwrap().to_string().contains("segment"), "authority changes still refuse");
}
