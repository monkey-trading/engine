//! The node config `scripts/snp-local-chain.sh` writes for an SEV-SNP guest, through
//! `scripts/snp-node-config.py`, loads and validates as a node config with real attestation: the guest
//! seals its keys under `/data`, takes delivered secrets, names the Genoa certificate chain and the
//! Siena CPU model a peer guest's quote may come from, reaches
//! the chain only over HTTPS at the proxy's ports, credits deposits at `latest`, and posts checkpoints
//! through its relayer. A change to `config/anvil.toml` that the script's edits no longer match fails
//! here, as the script does.

use std::path::Path;
use std::process::Command;

use engine_node::NodeConfig;
use engine_node::config::{FinalityTag, TeeProvider};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/snp-node-config.py");
const ANVIL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/anvil.toml");
const HOST: &str = "chain.snp.test";
const VAULT: &str = "0x00000000000000000000000000000000000000a1";
const USDC: &str = "0x00000000000000000000000000000000000000a2";
/// Anvil's account 3, which the script names the first guest's checkpoint relayer.
const RELAYER: &str = "0x90F79bf6EB2c4f870365E785982E1f101E93b906";

#[test]
fn the_guest_config_the_local_chain_script_writes_loads_with_real_attestation() {
    // A fixed operator key that config/test-keys.toml does not list, as the script makes a fresh one.
    let operator = k256::ecdsa::SigningKey::from_slice(&[0x5a; 32]).unwrap();
    let public_key = format!("0x{}", hex::encode(operator.verifying_key().to_sec1_point(false).as_bytes()));
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("snp-guest-config");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("node.toml");
    let written = Command::new("python3")
        .arg("-I")
        .arg(SCRIPT)
        .arg(ANVIL)
        .arg(&path)
        .args([HOST, VAULT, USDC, RELAYER, &public_key, "Genoa", "Genoa-A2", "18545", "18546", "18547", "18548"])
        .output()
        .expect("python3 runs");
    assert!(written.status.success(), "the script failed: {}", String::from_utf8_lossy(&written.stderr));

    let cfg = NodeConfig::load(&path).unwrap_or_else(|err| panic!("the guest config does not load: {err:#}"));
    assert_eq!(cfg.tee.provider, TeeProvider::ConfigfsTsm);
    assert_eq!(cfg.tee.processor.as_deref(), Some("Genoa"));
    let models: Vec<_> = cfg.tee.cpu_models.iter().map(|m| (m.product_name.as_str(), m.cpuid())).collect();
    assert_eq!(models, [("Genoa-A2", engine_verify::Cpuid { family: 0x19, model: 0xA0, stepping: 0x02 })]);
    let sealing = cfg.tee.sealing.as_ref().expect("[tee.sealing]");
    assert_eq!(sealing.dir, Path::new("/data/keys"));
    assert_eq!(sealing.secrets.as_ref().expect("[tee.sealing.secrets]").listen, "0.0.0.0:9472".parse().unwrap());
    assert!(cfg.alerts.critical.as_ref().expect("[alerts.critical]").url_secret.is_some(), "a delivered route");
    assert_eq!(cfg.operator.public_key, public_key);
    assert_eq!(
        (cfg.node.listen, cfg.metrics.listen),
        ("0.0.0.0:8080".parse().unwrap(), "0.0.0.0:9464".parse().unwrap())
    );
    assert!(cfg.node.log_dir.starts_with("/data"), "the image's one writable disk: {:?}", cfg.node.log_dir);

    assert_eq!(cfg.custody.vault, VAULT);
    let evm = cfg.custody.evm.as_ref().expect("EVM custody");
    assert!(evm.usdc.to_string().eq_ignore_ascii_case(USDC));
    assert_eq!(evm.finality_tag, FinalityTag::Latest);
    let providers: Vec<String> =
        evm.providers.iter().map(|provider| provider.rpc_url.expose().unwrap().into()).collect();
    assert_eq!(providers, ["18545", "18546", "18547"].map(|port| format!("https://{HOST}:{port}/")));
    let checkpoints = cfg.checkpoints.as_ref().expect("[checkpoints]");
    assert_eq!(checkpoints.rpc_url.expose().unwrap().as_str(), format!("https://{HOST}:18548/"));
    assert!(checkpoints.relayer.to_string().eq_ignore_ascii_case(RELAYER));
}
