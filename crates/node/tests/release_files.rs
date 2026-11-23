//! The configurations and manifest a release ships, as `scripts/release-files.py` writes them for the
//! release workflow: one configuration for each file under `config/release/`. Each loads as a node
//! config and holds what a production build requires, and the manifest parses as
//! `engine verify-attestation` reads it, pinning each configuration's SHA-256 and the production
//! configuration's CPU models. Staging differs from production only in what each deployment sets for
//! itself, and production differs from the template only in those values, the host's product names,
//! the rotation contracts it names, and `[snapshots]`. The script refuses a measurement of the dev image
//! or without Secure TSC, configurations whose CPU models differ, and a tag whose configurations still
//! carry a marker.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use engine_node::NodeConfig;
use engine_verify::ReleaseManifest;

#[path = "common/production_config.rs"]
mod production_config;

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const APPENDED: [&str; 2] = ["config/benchmark-publishers.toml", "config/production-genesis.toml"];
/// A production image's measurement.json, as `scripts/build-image.sh` writes it.
const MEASUREMENT: &str = r#"{
  "cmdline": "console=ttyS0 ip=dhcp panic=-1 no-kvmclock",
  "guest_features": "0x201",
  "launch_digest": "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f30",
  "policy": "0x30000",
  "variant": "production",
  "vcpu_type": "EPYC-v4",
  "vcpus": 4,
  "vmm_type": "QEMU"
}"#;
/// The values each deployment sets for itself: its chain, contracts, accounts, providers and domain.
const DEPLOYMENT: [&str; 35] = [
    "api.allowed_origins",
    "api.tls.acme.domain",
    "api.tls.acme.contact",
    "tee.sealing.handover.mirrors",
    "operator.public_key",
    "custody.chain_id",
    "custody.vault",
    "custody.evm.providers",
    "custody.evm.usdc",
    "custody.evm.minter",
    "custody.evm.start_block",
    "custody.evm.staking.contract",
    "custody.evm.staking.registry.contract",
    "custody.evm.staking.registry.lens",
    "custody.evm.staking.registry.schedule",
    "custody.evm.staking.registry.vote.contract",
    "custody.evm.staking.registry.fault_proof.contract",
    "checkpoints.rpc_url",
    "checkpoints.relayer",
    "checkpoints.relayer_gas.min_balance",
    "log_heads.contract",
    "log_heads.relayer",
    "liveness.caller",
    "liveness.min_balance",
    "seed_relay.providers",
    "seed_relay.chain_id",
    "seed_relay.rpc_url",
    "seed_relay.relayer",
    "seed_relay.beacon_api",
    "seed_relay.relay",
    "seed_relay.receiver",
    "seed_relay.min_balance",
    "seed_relay.l2_max_fee_per_gas",
    "access.source",
    "access.endpoint",
];
/// Values each deployment sets for itself when its config holds them: a later release's recovery caches
/// pin the enclave its own running release attests.
const DEPLOYMENT_IF_SET: [&str; 1] = ["snapshots.trusted_signers"];

#[test]
fn each_release_config_loads_and_the_manifest_pins_it() {
    let out = scratch("loads");
    let written = release_files(Path::new(ROOT), &out, &[]);
    assert!(written.status.success(), "{}", String::from_utf8_lossy(&written.stderr));

    let manifest = ReleaseManifest::from_toml(&fs::read_to_string(out.join("release.toml")).unwrap()).unwrap();
    assert_eq!(manifest.name, "v0.1.0");
    let measurement: serde_json::Value = serde_json::from_str(MEASUREMENT).unwrap();
    assert_eq!(hex::encode(manifest.launch_digest), measurement["launch_digest"].as_str().unwrap());
    assert_eq!((manifest.policy, manifest.guest_features), (0x30000, 0x201));
    assert_eq!(manifest.config_hashes.len(), environments().len());
    let mut chains = BTreeMap::new();
    for environment in environments() {
        let file = format!("{environment}.toml");
        // The released file is the environment's sections followed by the publishers and the genesis.
        let text = fs::read_to_string(out.join(&file)).unwrap();
        assert!(text == assembled(&environment), "{file} is not its sections, the publishers and the genesis");
        let cfg = NodeConfig::load(&out.join(&file)).unwrap_or_else(|err| panic!("{file}: {err:#}"));
        production_config::assert_holds_what_a_production_build_requires(&cfg, &text);
        let unmarked = production_config::unmarked_placeholders(&text, &production_config::PLACEHOLDER_VALUES);
        assert_eq!(unmarked, Vec::<String>::new(), "{file}");
        assert_eq!(production_config::unlisted_placeholders(&text), Vec::<String>::new(), "{file}");
        assert_eq!(manifest.config_name(&cfg.file_sha256), Some(file.as_str()));
        assert_eq!(manifest.cpu_models, cfg.tee.cpu_models, "{file}: a verifier takes the models a peer does");
        let seeds = cfg.seed_relay.as_ref().expect("[seed_relay]").chain_id;
        chains.insert(environment, (cfg.custody.chain_id, seeds));
    }
    // Production on Arbitrum One, relaying seeds from Ethereum; staging on Arbitrum Sepolia, from Sepolia.
    assert_eq!(chains["production"], (42_161, 1));
    assert_eq!(chains["staging"], (421_614, 11_155_111));
}

#[test]
fn staging_differs_from_production_only_in_what_a_deployment_sets() {
    let differ = deployment_differences(&read("config/release/production.toml"), &read("config/release/staging.toml"));
    assert_eq!(differ, Vec::<String>::new());
}

/// Beyond what a deployment sets, production may differ from the template in its host's product names,
/// in the rotation contracts the template leaves commented out, and in `[snapshots]`, which a first
/// release leaves out and a later one adds with its enclave address.
#[test]
fn production_differs_from_the_template_only_in_what_a_deployment_sets() {
    let template = leaves(&read("ops/runbooks/production-node.toml"));
    let production = leaves(&read("config/release/production.toml"));
    let rotation = ["custody.evm.staking.registry.vote.", "custody.evm.staking.registry.fault_proof."];
    for prefix in rotation {
        assert!(production.keys().any(|path| path.starts_with(prefix)), "{prefix}");
    }
    let host = ["tee.processor", "tee.cpu_models"];
    let except: Vec<&str> = DEPLOYMENT.into_iter().chain(host).chain(rotation).chain(["snapshots."]).collect();
    assert_eq!(differences(&template, &production, &except), Vec::<String>::new());
}

/// A deployment that sets every value and deletes every marker, as the runbook says, passes CI's checks
/// and a tag's: no placeholder is left unmarked, each config loads, and `--final` writes the manifest. So
/// does a later release that adds `[snapshots]` to each configuration, pinning its own enclave. It holds
/// whatever values the configurations have set so far. A placeholder whose marker is deleted is still
/// caught, as the template, which always holds them, shows.
#[test]
fn a_release_config_with_every_value_set_passes_ci_and_a_tag() {
    let template: String =
        read("ops/runbooks/production-node.toml").lines().map(|line| format!("{}\n", unmarked(line))).collect();
    let caught = production_config::unmarked_placeholders(&template, &production_config::PLACEHOLDER_VALUES);
    for placeholder in ["vault = \"0x1111", "start_block = 1", "caller = \"0xcccc", "public_key = \"0x045a"] {
        assert!(caught.iter().any(|line| line.contains(placeholder)), "{placeholder}: {caught:?}");
    }

    let repo = scratch("set-repo");
    for environment in environments() {
        let path = format!("config/release/{environment}.toml");
        let unmarked_text: String = read(&path).lines().map(|line| format!("{}\n", unmarked(line))).collect();
        let set = set_values(&unmarked_text);
        let left = production_config::unmarked_placeholders(&set, &production_config::PLACEHOLDER_VALUES);
        assert_eq!(left, Vec::<String>::new(), "{path}");
        write(&repo, &path, &set);
    }
    for path in APPENDED {
        write(&repo, path, &read(path));
    }
    let first = scratch("set");
    let tag = release_files(&repo, &first, &["--final"]);
    assert!(tag.status.success(), "{}", String::from_utf8_lossy(&tag.stderr));
    for environment in environments() {
        let file = first.join(format!("{environment}.toml"));
        NodeConfig::load(&file).unwrap_or_else(|err| panic!("{environment}.toml: {err:#}"));
    }

    // A later release: each configuration pins the enclave address its own running release attests.
    for (n, environment) in environments().iter().enumerate() {
        let path = repo.join(format!("config/release/{environment}.toml"));
        let signer = format!("0x{:040x}", 0xa1 + n);
        let section = format!(
            "\n[snapshots]\ndir = \"/data/snapshots\"\nretain = 2\ninterval = \"5m\"\nmax_bytes = 2147483648\ntrusted_signers = [\"{signer}\"]\n"
        );
        fs::write(&path, format!("{}{section}", without_snapshots(&fs::read_to_string(&path).unwrap()))).unwrap();
    }
    let read_set =
        |environment: &str| fs::read_to_string(repo.join(format!("config/release/{environment}.toml"))).unwrap();
    assert_eq!(deployment_differences(&read_set("production"), &read_set("staging")), Vec::<String>::new());
    let later = scratch("set-later");
    let tag = release_files(&repo, &later, &["--final"]);
    assert!(tag.status.success(), "{}", String::from_utf8_lossy(&tag.stderr));
    for environment in environments() {
        let cfg = NodeConfig::load(&later.join(format!("{environment}.toml"))).unwrap();
        assert_eq!(cfg.snapshots.expect("[snapshots]").trusted_signers.len(), 1, "{environment}");
    }
}

#[test]
fn the_script_refuses_a_dev_image_no_secure_tsc_unlike_cpu_models_and_a_tags_marker() {
    let refused = |output: Output, why: &str| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success() && stderr.contains(why), "{why}: {stderr}");
    };
    let root = Path::new(ROOT);
    let dev = MEASUREMENT.replace(r#""variant": "production""#, r#""variant": "dev""#);
    refused(release_files_measured(root, &scratch("dev"), &[], &dev), "not the production one");
    let no_secure_tsc = MEASUREMENT.replace(r#""guest_features": "0x201""#, r#""guest_features": "0x1""#);
    refused(release_files_measured(root, &scratch("tsc"), &[], &no_secure_tsc), "lack SNPActive and SecureTSC");

    // A copy of the repository's configurations with every marker taken off, as a tag's must be.
    let repo = scratch("repo");
    for environment in environments() {
        let path = format!("config/release/{environment}.toml");
        let unmarked: String = read(&path).lines().map(|line| format!("{}\n", unmarked(line))).collect();
        write(&repo, &path, &unmarked);
    }
    for path in APPENDED {
        write(&repo, path, &read(path));
    }
    let tag = release_files(&repo, &scratch("final"), &["--final"]);
    assert!(tag.status.success(), "{}", String::from_utf8_lossy(&tag.stderr));

    // One marker left refuses the tag, naming its line; a dry run only warns.
    let staging = repo.join("config/release/staging.toml");
    let text = fs::read_to_string(&staging).unwrap();
    fs::write(&staging, edit_line(&text, "confirmations = ", |line| format!("{line} # OWNER"))).unwrap();
    refused(release_files(&repo, &scratch("marked"), &["--final"]), "staging.toml:");
    let dry_run = release_files(&repo, &scratch("dry-run"), &[]);
    assert!(dry_run.status.success() && String::from_utf8_lossy(&dry_run.stderr).contains("warning"));

    // A staging host that takes another CPU model than production's.
    fs::write(&staging, edit_line(&text, "stepping = ", |_| "stepping = 0x7F".into())).unwrap();
    refused(release_files(&repo, &scratch("models"), &[]), "differ from production.toml's");
}

#[test]
fn the_script_refuses_a_malformed_name_or_field_and_a_repository_without_production() {
    let refused = |output: Output, why: &str| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success() && stderr.contains(why), "{why}: {stderr}");
    };
    let root = Path::new(ROOT);
    let named = |name: &str, label: &str| release_files_named(root, &scratch(label), &[], MEASUREMENT, name);
    refused(named("v1\"0", "quote"), "a release name is letters, digits, dots, dashes and underscores");
    let digest = "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f30";
    let short = MEASUREMENT.replace(digest, &digest[..95]);
    refused(release_files_measured(root, &scratch("digits"), &[], &short), "launch_digest is not 48 bytes");
    let not_hex = MEASUREMENT.replace(r#""policy": "0x30000""#, r#""policy": "0xzz""#);
    refused(release_files_measured(root, &scratch("policy"), &[], &not_hex), "policy is not hex");

    let bare = scratch("bare");
    write(&bare, "config/release/staging.toml", &read("config/release/staging.toml"));
    refused(release_files(&bare, &scratch("no-production"), &[]), "holds no production.toml");
}

// ── Helpers ──

/// The deployments a release ships: each file under `config/release/`, by name.
fn environments() -> Vec<String> {
    let dir = Path::new(ROOT).join("config/release");
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "toml"))
        .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The paths at which `other`'s node sections differ from `production`'s, apart from what a deployment
/// sets for itself, after checking that both hold each of `DEPLOYMENT`.
fn deployment_differences(production: &str, other: &str) -> Vec<String> {
    let (production, other) = (leaves(production), leaves(other));
    for path in DEPLOYMENT {
        assert!(production.contains_key(path) && other.contains_key(path), "{path} is in both");
    }
    let except: Vec<&str> = DEPLOYMENT.into_iter().chain(DEPLOYMENT_IF_SET).collect();
    differences(&production, &other, &except)
}

/// Runs the script on `repo` as the release workflow does, with the production measurement.
fn release_files(repo: &Path, out: &Path, flags: &[&str]) -> Output {
    release_files_measured(repo, out, flags, MEASUREMENT)
}

fn release_files_measured(repo: &Path, out: &Path, flags: &[&str], measurement: &str) -> Output {
    release_files_named(repo, out, flags, measurement, "v0.1.0")
}

fn release_files_named(repo: &Path, out: &Path, flags: &[&str], measurement: &str, name: &str) -> Output {
    let measured = out.with_extension("measurement.json");
    fs::write(&measured, measurement).unwrap();
    Command::new("python3")
        .arg("-I")
        .arg(Path::new(ROOT).join("scripts/release-files.py"))
        .args(flags)
        .arg("--repo")
        .arg(repo)
        .arg(name)
        .arg(&measured)
        .arg(out)
        .output()
        .expect("python3 runs")
}

/// A directory of this test binary's own, emptied, under a name for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("release-files-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn read(path: &str) -> String {
    fs::read_to_string(Path::new(ROOT).join(path)).unwrap()
}

fn write(repo: &Path, path: &str, text: &str) {
    let path = repo.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// `cat config/release/<environment>.toml config/benchmark-publishers.toml config/production-genesis.toml`.
fn assembled(environment: &str) -> String {
    [format!("config/release/{environment}.toml")]
        .into_iter()
        .chain(APPENDED.map(String::from))
        .map(|p| read(&p))
        .collect()
}

/// `text` with each placeholder value replaced by one a deployment could set: another domain, another
/// address of the same length, an operator key whose private key exists, and a later start block.
fn set_values(text: &str) -> String {
    let operator = k256::ecdsa::SigningKey::from_slice(&[0x5a; 32]).unwrap();
    let key = format!("public_key = \"0x{}\"", hex::encode(operator.verifying_key().to_sec1_point(false).as_bytes()));
    let mut set: String = text
        .lines()
        .map(
            |line| if line.starts_with("public_key = \"0x045a1ac0") { format!("{key}\n") } else { format!("{line}\n") },
        )
        .collect();
    set = set.replace("example.com", "trading.test").replace("start_block = 1\n", "start_block = 250000000\n");
    for placeholder in production_config::PLACEHOLDER_VALUES.iter().filter(|value| value.starts_with("0x")) {
        set = set.replace(placeholder, &format!("0xc0ffee{}", &placeholder[8..]));
    }
    set
}

/// `text` with the first line that starts with `prefix` passed through `edit`.
fn edit_line(text: &str, prefix: &str, edit: impl Fn(&str) -> String) -> String {
    let at = text.lines().position(|line| line.starts_with(prefix)).unwrap_or_else(|| panic!("no {prefix}"));
    text.lines().enumerate().map(|(n, line)| if n == at { edit(line) + "\n" } else { format!("{line}\n") }).collect()
}

/// `text` without its `[snapshots]` table, from its header to the next table's.
fn without_snapshots(text: &str) -> String {
    let mut inside = false;
    let mut kept = String::new();
    for line in text.lines() {
        if line.starts_with('[') {
            inside = line == "[snapshots]";
        }
        if !inside {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    kept
}

/// `line` without its trailing marker comment.
fn unmarked(line: &str) -> &str {
    ["# OWNER", "# REPLACE"]
        .iter()
        .filter_map(|marker| line.find(marker))
        .min()
        .map_or(line, |at| line[..at].trim_end())
}

/// Every value in a config by its dotted path, descending into tables but not into arrays.
fn leaves(text: &str) -> BTreeMap<String, toml::Value> {
    fn descend(prefix: &str, table: &toml::Table, out: &mut BTreeMap<String, toml::Value>) {
        for (key, value) in table {
            let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
            match value {
                toml::Value::Table(inner) => descend(&path, inner, out),
                _ => {
                    out.insert(path, value.clone());
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    descend("", &toml::from_str(text).unwrap(), &mut out);
    out
}

/// The paths at which `a` and `b` differ, or which only one holds, other than those `except` names
/// whole or, ending in a dot, by prefix.
fn differences(a: &BTreeMap<String, toml::Value>, b: &BTreeMap<String, toml::Value>, except: &[&str]) -> Vec<String> {
    let excepted = |path: &str| except.iter().any(|e| path == *e || (e.ends_with('.') && path.starts_with(e)));
    let paths: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    paths.into_iter().filter(|path| !excepted(path) && a.get(*path) != b.get(*path)).cloned().collect()
}
