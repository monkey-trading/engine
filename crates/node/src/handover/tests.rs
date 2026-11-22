//! The handover's pieces: what each step binds, the encryption of the keys, the framing, and what
//! each kind of evidence proves. `predecessor`'s tests run the whole handover over loopback.

mod archive;

use super::*;

use crate::config::NodeConfig;

use engine_core::types::AccountId;
use engine_verify::fixtures::{
    LAUNCH_DIGEST, POLICY, REPORTED_TCB, collateral_of, cpu_model, crl_revoking, forged_report, pem_wrap, test_chain,
};
use engine_verify::{CpuModel, MinimumTcb, Tcb};
use hpke::Serializable;

use crate::config::TeeProvider;
use crate::handover::evidence::{Peer, Quoting, same_policy, verify};
use crate::tee::MockAttestor;
use crate::tee::sealing::Platform;

const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31337, vault: AccountId([0x0a; 20]) };
const CONFIG: [u8; 32] = [0xc1; 32];
const CHALLENGE: [u8; 32] = [0x5c; 32];
/// The newest protocol version the successor's build reads.
const READS: &str = "v51";

#[test]
fn the_keys_open_only_for_the_requested_key_deployment_and_request() {
    let keys = KeySet::generate().unwrap();
    let (secret, public) = Kx::gen_keypair();
    let request = request_report_data(DOMAIN, &CHALLENGE, &public.to_bytes(), &CONFIG, READS);
    let (encapped, ciphertext) = seal_keys(DOMAIN, &public.to_bytes(), &request, &keys).unwrap();
    assert_eq!(open_keys(DOMAIN, &secret, &request, &encapped, &ciphertext).unwrap(), keys);

    let (other_secret, _) = Kx::gen_keypair();
    let other_vault = CheckpointDomain { vault: AccountId([0x0b; 20]), ..DOMAIN };
    let mut other_request = request;
    other_request[0] ^= 1;
    let mut tampered = ciphertext.clone();
    tampered[0] ^= 1;
    for (what, opened) in [
        ("another secret key", open_keys(DOMAIN, &other_secret, &request, &encapped, &ciphertext)),
        ("another deployment", open_keys(other_vault, &secret, &request, &encapped, &ciphertext)),
        ("another request", open_keys(DOMAIN, &secret, &other_request, &encapped, &ciphertext)),
        ("an altered ciphertext", open_keys(DOMAIN, &secret, &request, &encapped, &tampered)),
    ] {
        assert!(opened.is_err(), "the keys opened under {what}");
    }
    assert!(seal_keys(DOMAIN, &[1, 2, 3], &request, &keys).is_err(), "sealed to a malformed key");
}

#[test]
fn each_step_binds_its_deployment_and_what_it_answers() {
    // The request binds the versions the successor reads: a host cannot claim it reads newer ones.
    let (key, other_key) = ([1u8; 32], [2u8; 32]);
    let other_chain = CheckpointDomain { chain_id: 1, ..DOMAIN };
    let other_vault = CheckpointDomain { vault: AccountId([0x0b; 20]), ..DOMAIN };
    let request = request_report_data(DOMAIN, &CHALLENGE, &key, &CONFIG, READS);
    assert_eq!(request[32..], [0u8; 32], "the second half of report data is zero");
    for other in [
        request_report_data(DOMAIN, &CHALLENGE, &other_key, &CONFIG, READS),
        request_report_data(DOMAIN, &CHALLENGE, &key, &[0xc2; 32], READS),
        request_report_data(DOMAIN, &CHALLENGE, &key, &CONFIG, "v52"),
        request_report_data(DOMAIN, &[0x5d; 32], &key, &CONFIG, READS),
        request_report_data(other_chain, &CHALLENGE, &key, &CONFIG, READS),
        request_report_data(other_vault, &CHALLENGE, &key, &CONFIG, READS),
    ] {
        assert_ne!(other, request);
    }

    let keys = keys_report_data(DOMAIN, &request, b"enc", b"cipher");
    assert_eq!(keys[32..], [0u8; 32]);
    assert_ne!(keys, request, "the two steps share a tag");
    for other in [
        keys_report_data(
            DOMAIN,
            &request_report_data(DOMAIN, &CHALLENGE, &other_key, &CONFIG, READS),
            b"enc",
            b"cipher",
        ),
        keys_report_data(DOMAIN, &request, b"enX", b"cipher"),
        keys_report_data(DOMAIN, &request, b"enc", b"cipheX"),
        keys_report_data(other_vault, &request, b"enc", b"cipher"),
    ] {
        assert_ne!(other, keys);
    }

    let confirm = confirm_digest(DOMAIN, &request);
    assert_ne!(confirm, confirm_digest(other_chain, &request));
    assert_ne!(confirm, confirm_digest(DOMAIN, &request_report_data(DOMAIN, &CHALLENGE, &other_key, &CONFIG, READS)));
}

#[tokio::test]
async fn a_message_over_the_limit_or_cut_short_is_refused() {
    let (mut client, mut server) = tokio::io::duplex(64);
    send(&mut client, &Confirm { signature: "0xab".into() }).await.unwrap();
    assert_eq!(receive::<Confirm>(&mut server).await.unwrap().signature, "0xab");

    let (mut client, mut server) = tokio::io::duplex(64);
    client.write_all(&(MAX_MESSAGE_BYTES + 1).to_be_bytes()).await.unwrap();
    let err = receive::<Confirm>(&mut server).await.unwrap_err();
    assert!(err.to_string().contains("over the limit"), "{err:#}");

    let (mut client, mut server) = tokio::io::duplex(64);
    client.write_all(&10u32.to_be_bytes()).await.unwrap();
    client.write_all(b"{}").await.unwrap();
    drop(client);
    assert!(receive::<Confirm>(&mut server).await.is_err(), "a message cut short parsed");
}

#[tokio::test]
async fn mock_evidence_binds_its_data_and_never_passes_for_a_real_enclave() {
    let platform = Platform { launch_digest: [0x22; 48], policy: 0, tcb: 0 };
    let quoting = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "https://kds.test" };
    let report_data = request_report_data(DOMAIN, &CHALLENGE, &[7u8; 32], &CONFIG, READS);
    let evidence = evidence::produce(&quoting, &MockAttestor, &platform, report_data).await.unwrap();

    let peer = verify(TeeProvider::Mock, &[], &evidence, &report_data, None, None).unwrap();
    assert_eq!(peer, Peer { launch_digest: [0x22; 48], policy: 0 });
    assert!(verify(TeeProvider::Mock, &[], &evidence, &[0u8; 64], None, None).is_err(), "it verified other data");
    let err = verify(TeeProvider::ConfigfsTsm, &[cpu_model()], &evidence, &report_data, Some(0), None).unwrap_err();
    assert!(err.to_string().contains("never hands over to a mock one"), "{err:#}");
}

#[test]
fn hardware_evidence_verifies_against_the_vendor_chain_and_binds_its_data() {
    let (evidence, report_data, root) = hardware_evidence();
    let models = [cpu_model()];
    let now = Some(1_800_000_000);

    let peer = verify(TeeProvider::ConfigfsTsm, &models, &evidence, &report_data, now, Some(&root)).unwrap();
    assert_eq!(peer, Peer { launch_digest: LAUNCH_DIGEST, policy: POLICY });
    let hardware = |data: &[u8; 64], now: Option<i64>, root: Option<&str>| {
        verify(TeeProvider::ConfigfsTsm, &models, &evidence, data, now, root)
    };
    assert!(hardware(&[0u8; 64], now, Some(&root)).is_err(), "other data");
    assert!(hardware(&report_data, now, None).is_err(), "an unpinned root");
    assert!(hardware(&report_data, None, Some(&root)).is_err(), "no time");
    let err = verify(TeeProvider::Mock, &models, &evidence, &report_data, None, None).unwrap_err();
    assert!(err.to_string().contains("mock enclaves only"), "{err:#}");
}

#[test]
fn a_hardware_peer_is_held_to_its_own_models_floor_and_never_to_this_platforms_tcb() {
    let (evidence, report_data, root) = hardware_evidence();
    let now = Some(1_800_000_000);
    let peer =
        |models: &[CpuModel]| verify(TeeProvider::ConfigfsTsm, models, &evidence, &report_data, now, Some(&root));

    // At its model's floor: admitted, and no weaker than a platform committed above its TCB.
    let admitted = peer(&[cpu_model()]).expect("the peer's model at its floor");
    let ahead = Tcb { microcode: REPORTED_TCB.microcode + 1, ..REPORTED_TCB };
    let own = Platform { launch_digest: LAUNCH_DIGEST, policy: POLICY, tcb: ahead.to_packed() };
    same_policy(&own, &admitted).expect("the holder's own TCB is no floor");
    // A model the table does not list, and its own model's floor above its TCB, refuse it.
    let err = peer(&[]).unwrap_err();
    assert!(err.to_string().ends_with("is not a model the release accepts"), "{err:#}");
    let above = MinimumTcb { microcode: REPORTED_TCB.microcode + 1, ..cpu_model().minimum_tcb };
    let err = peer(&[CpuModel { minimum_tcb: above, ..cpu_model() }]).unwrap_err();
    assert!(err.to_string().starts_with("the quote's reported TCB"), "{err:#}");
}

/// A hardware peer's evidence over a handover request's report data, from the fixture chip, with the
/// fixture chain's root to pin.
fn hardware_evidence() -> (Evidence, [u8; 64], String) {
    let chip = [0x5a; 64];
    let chain = test_chain(chip, REPORTED_TCB);
    let report_data = request_report_data(DOMAIN, &CHALLENGE, &[7u8; 32], &CONFIG, READS);
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let evidence: Evidence = serde_json::from_value(serde_json::json!({
        "provider": "sev_guest",
        "quote": hex::encode(forged_report(&chain, LAUNCH_DIGEST, report_data, chip)),
        "collateral": String::from_utf8(collateral.to_json()).unwrap(),
    }))
    .unwrap();
    (evidence, report_data, pem_wrap(&collateral.root, "CERTIFICATE"))
}

/// Every signer epoch the vault records is read, however many, so a copy's replay accepts the seals of
/// each; a count past the bound, or an epoch whose read fails, is refused rather than read in part, and a
/// chain without lifecycle evidence names its current signer only.
#[tokio::test]
async fn every_recorded_signer_epoch_is_read() {
    let signer_of = |epoch: u64| {
        let mut address = [0u8; 20];
        address[12..].copy_from_slice(&epoch.to_be_bytes());
        AccountId(address)
    };
    let observed = |epochs: u64| crate::checkpoints::Observation {
        view: crate::checkpoints::View::Finalized,
        state: crate::checkpoints::VaultState {
            height: 10,
            signer: signer_of(epochs),
            checkpoint: None,
            balance: engine_core::Fixed::ZERO,
            deposits: 0,
            deposited: engine_core::Fixed::ZERO,
            distributions_paid: engine_core::Fixed::ZERO,
        },
        lifecycle: Some(crate::checkpoints::LifecycleEvidence {
            height: 10,
            block_hash: engine_core::eip712::B256::repeat_byte(10),
            signer: signer_of(epochs),
            signer_epoch: epochs,
            withdrawals_paused: false,
            total_withdrawn: engine_core::Fixed::ZERO,
            checkpoint_seq: None,
            checkpoint_root: None,
            approvals: Vec::new(),
        }),
    };
    let epochs = Epochs(Box::new(move |epoch| Some(signer_of(epoch))));
    let signers = vault_signers(&epochs, &observed(300)).await.unwrap();
    assert_eq!(signers, (1..=300).map(signer_of).collect(), "a recorded epoch's signer is missing");
    let refused = vault_signers(&epochs, &observed(MAX_SIGNER_EPOCHS + 1)).await.unwrap_err();
    assert!(refused.to_string().contains("signer epochs"), "{refused:#}");
    let failing = Epochs(Box::new(move |epoch| (epoch != 2).then(|| signer_of(epoch))));
    let failed = vault_signers(&failing, &observed(3)).await.unwrap_err();
    assert!(format!("{failed:#}").contains("epoch 2"), "a failed read left its signer out: {failed:#}");
    let unproven = crate::checkpoints::Observation { lifecycle: None, ..observed(3) };
    let unread = Epochs(Box::new(|_| None));
    assert_eq!(vault_signers(&unread, &unproven).await.unwrap(), [signer_of(3)].into(), "no lifecycle evidence");
}

/// A chain whose every signer epoch reads back the signer the test names for it, or fails where it names none.
struct Epochs(Box<dyn Fn(u64) -> Option<AccountId> + Send + Sync>);

impl CheckpointChain for Epochs {
    async fn state(&self, _view: crate::checkpoints::View) -> anyhow::Result<crate::checkpoints::VaultState> {
        anyhow::bail!("this chain reads no vault state")
    }

    async fn post(&self, _checkpoint: &crate::log::Checkpoint) -> anyhow::Result<()> {
        anyhow::bail!("this chain posts nothing")
    }

    async fn epoch_signers(
        &self,
        _at: &crate::checkpoints::Observation,
        epochs: &[u64],
    ) -> anyhow::Result<Vec<AccountId>> {
        epochs.iter().map(|epoch| (self.0)(*epoch).with_context(|| format!("epoch {epoch} unread"))).collect()
    }
}

/// A chain whose handover source and named hostname the test holds.
struct Registry {
    source: crate::checkpoints::HandoverSource,
    hostname: String,
}

impl CheckpointChain for Registry {
    async fn state(&self, _view: crate::checkpoints::View) -> anyhow::Result<crate::checkpoints::VaultState> {
        anyhow::bail!("this chain reads no vault state")
    }

    async fn post(&self, _checkpoint: &crate::log::Checkpoint) -> anyhow::Result<()> {
        anyhow::bail!("this chain posts nothing")
    }

    async fn handover_source(&self) -> anyhow::Result<crate::checkpoints::HandoverSource> {
        Ok(self.source.clone())
    }

    async fn operator_hostname(&self) -> anyhow::Result<String> {
        Ok(self.hostname.clone())
    }
}

fn registry(source: crate::checkpoints::HandoverSource, hostname: &str) -> Registry {
    Registry { source, hostname: hostname.to_owned() }
}

fn source(candidate: AccountId, endpoint: &str) -> crate::checkpoints::HandoverSource {
    crate::checkpoints::HandoverSource {
        candidate,
        release: [0x22; 32],
        api_hostname: "api.operator.example".into(),
        handover_endpoint: endpoint.into(),
    }
}

#[test]
fn resolve_source_maps_the_registry_snapshot_and_refuses_the_unregistered() {
    let nobody = AccountId([0; 20]);
    let operator = AccountId([0x42; 20]);
    // No election yet: the start falls through to the usual refusal, not a handover.
    let none =
        tokio::runtime::Runtime::new().unwrap().block_on(resolve_source(&registry(source(nobody, ""), ""))).unwrap();
    assert!(none.is_none(), "an election the registry never made is not a handover source");
    // An election whose incumbent registered no endpoint cannot hand over: the runbook's step.
    let unregistered = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(resolve_source(&registry(source(operator, ""), "")))
        .unwrap_err();
    assert!(format!("{unregistered:#}").contains("no handover endpoint"), "{unregistered:#}");
    // A recorded source resolves.
    let resolved = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(resolve_source(&registry(source(operator, "host:9443"), "")))
        .unwrap()
        .expect("a recorded handover source resolves");
    assert_eq!(resolved.endpoint, "host:9443");
    assert_eq!(resolved.api_hostname, "api.operator.example");
    assert_eq!(resolved.candidate, operator);
    assert_eq!(resolved.release, [0x22; 32]);
}

#[tokio::test]
async fn check_operator_hostname_refuses_a_mismatch_and_skips_without_acme() {
    // The dev config has no [api.tls.acme]: no certificate to check.
    let cfg = crate::test_support::dev_config();
    check_operator_hostname(&cfg, &registry(source(AccountId([0x42; 20]), ""), "other.example"), None).await.unwrap();
    let mut value: toml::Value =
        toml::from_str(&std::fs::read_to_string(crate::test_support::DEV_CONFIG).unwrap()).unwrap();
    let tls = toml::Value::try_from(
        serde_json::json!({ "acme": { "domain": "api.operator.example", "directory_url": "https://ca.example/dir", "contact": "mailto:ops@example.com" } }),
    )
    .unwrap();
    value.get_mut("api").expect("the dev config's api table").as_table_mut().unwrap().insert("tls".into(), tls);
    let cfg: NodeConfig = toml::from_str(&toml::to_string(&value).unwrap()).unwrap();
    check_operator_hostname(&cfg, &registry(source(AccountId([0x42; 20]), ""), "api.operator.example"), None)
        .await
        .unwrap();
    let err = check_operator_hostname(&cfg, &registry(source(AccountId([0x42; 20]), ""), "other.example"), None)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("other.example"), "{err:#}");
}

/// Collateral fetched for a chip and TCB is reused for the reuse time, and fetched again for another
/// chip or TCB or once that time has passed.
#[tokio::test]
async fn collateral_is_reused_for_its_chip_and_tcb_until_the_reuse_time_passes() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fetches = AtomicUsize::new(0);
    let fetch = || async { Ok(format!("collateral {}", fetches.fetch_add(1, Ordering::SeqCst))) };
    let tcb = engine_verify::Tcb::from_packed(0x0102_0304_0506_0708);
    let (chip, other_chip) = ([1; 64], [2; 64]);
    let cache = evidence::CollateralCache::new(std::time::Duration::from_millis(300));
    assert_eq!(cache.json(chip, tcb, fetch).await.unwrap(), "collateral 0");
    assert_eq!(cache.json(chip, tcb, fetch).await.unwrap(), "collateral 0", "refetched within the reuse time");
    assert_eq!(cache.json(other_chip, tcb, fetch).await.unwrap(), "collateral 1", "another chip reused it");
    let lower = engine_verify::Tcb::from_packed(0x0102_0304_0506_0700);
    assert_eq!(cache.json(other_chip, lower, fetch).await.unwrap(), "collateral 2", "another TCB reused it");
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    assert_eq!(cache.json(other_chip, lower, fetch).await.unwrap(), "collateral 3", "kept past the reuse time");
    assert!(cache.json(chip, tcb, || async { anyhow::bail!("down") }).await.is_err());
}
