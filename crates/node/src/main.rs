//! `engine`: runs the node, replays a published log and verifies its checkpoints, verifies an
//! attestation against a release, or signs and submits an operator's admin command.

use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use engine_core::auth::{ClientAuth, Operator};
use engine_core::types::AccountId;
use engine_node::admin::{self, AdminArgs, HexKey, KeyEnv};
use engine_node::log::CheckpointDomain;
use engine_node::{
KDS_BASE_URL, NodeConfig, ReleaseManifest, VerifiedAttestation, VerifyOptions, ensure_signers_verified,
    init_logging, init_telemetry, load_attestation, replay_path_with_custody, replay_path_with_trust, run_dedicated,
    verify_attestation,
};

#[derive(Subcommand)]
enum Cmd {
    /// Run the node.
    Run {
        #[arg(short, long, env = "CONFIG_PATH", default_value = "config/dev.toml")]
        config: PathBuf,
    },
    /// Replay a published log and verify every checkpoint root and signature. `--log` is a single log file, or a
    /// directory of segments with their manifest: a node's log directory or a downloaded archive. A
    /// missing log, or one with no command or no checkpoint, fails.
    Replay {
        #[arg(short, long)]
        log: PathBuf,
        /// Explicit domain for signed logs written before domain recording.
        #[arg(long, requires = "vault")]
        chain_id: Option<u64>,
        #[arg(long, requires = "chain_id")]
        vault: Option<AccountId>,
        /// Pin an enclave signer obtained from a verified attestation or the vault.
        #[arg(long)]
        signer: Option<AccountId>,
        /// Verify historical signers against this deployment's finalized vault history.
        #[arg(long, requires = "signer", conflicts_with_all = ["chain_id", "vault", "attestation"])]
        custody_config: Option<PathBuf>,
        /// Require every checkpoint signer to pass attestation verification against `--release`.
        #[arg(long, requires = "release")]
        attestation: Option<PathBuf>,
        /// The release manifest `--attestation` verifies against.
        #[arg(long)]
        release: Option<PathBuf>,
        /// The vendor processor name for the certificate chain, as the vendor publishes it
        /// (the CPU generation's codename, for example Milan, Genoa, or Turin).
        #[arg(long, required = false)]
        processor: Option<String>,
        /// The vendor key-distribution service's base URL; the vendor's service unless overridden.
        #[arg(long, default_value_t = KDS_BASE_URL.to_owned())]
        kds: String,
        /// An extra root certificate, ASCII PEM, pinning the chain in addition to the compiled-in
        /// vendor roots. Test and private chains verify against this; release chains do not need it.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Verify an attestation served by `/v1/attestation` against a release manifest: the quote's
    /// certificate chain up to a pinned vendor root, the launch measurement, guest policy, and
    /// reported TCB against the release, and the report data's binding of the enclave address and
    /// a configuration the release lists. A mock quote never verifies.
    VerifyAttestation {
        /// The attestation JSON `/v1/attestation` served, saved to a file.
        #[arg(short, long)]
        attestation: PathBuf,
        /// The release manifest of measurements and configuration hashes.
        #[arg(short, long)]
        release: PathBuf,
        /// The vendor processor name for the certificate chain, as the vendor publishes it
        /// (the CPU generation's codename, for example Milan, Genoa, or Turin).
        #[arg(long, required = false)]
        processor: Option<String>,
        /// The vendor key-distribution service's base URL; the vendor's service unless overridden.
        #[arg(long, default_value_t = KDS_BASE_URL.to_owned())]
        kds: String,
        /// An extra root certificate, ASCII PEM, pinning the chain in addition to the compiled-in
        /// vendor roots. Test and private chains verify against this; release chains do not need it.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Sign an admin command with the operator's key and submit it to a node. The key comes from
    /// `--key-file`, or the `OPERATOR_KEY_FILE` or `OPERATOR_KEY` environment variable, never the
    /// command line.
    Admin(AdminArgs),
}

#[derive(Parser)]
#[command(about = "House-counterparty CFD engine, built to run inside a TEE")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Dispatches the subcommand.
/// Error cases:
/// - the config cannot be loaded, an alert webhook variable is unset or invalid, or the node stops
///   with an error
/// - the log is missing or cannot be read, fails its manifest check or verification, or holds no
///   command or no checkpoint
/// - an attestation does not verify against its release
/// - an admin command cannot be signed or submitted, or the engine rejects it
fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Run { config } => {
            let cfg = NodeConfig::load(&config)?;
            let telemetry = init_telemetry(&cfg, |var| std::env::var(var).ok())?;
            run_dedicated(cfg, telemetry)
        }
        Cmd::Replay { log: path, chain_id, vault, signer, custody_config, attestation, release, processor, kds, root } => {
            init_logging("info")?;
            let domain = chain_id.zip(vault).map(|(chain_id, vault)| CheckpointDomain { chain_id, vault });
            let mut replayed = if let Some(config) = custody_config {
                let cfg = NodeConfig::load(&config)?;
                let signer = signer.ok_or_else(|| anyhow::anyhow!("--custody-config requires --signer"))?;
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(replay_path_with_custody(&path, &cfg, signer))?
            } else {
                let replayed = replay_path_with_trust(&path, domain, signer)?;
                if let Some(signer) = signer {
                    anyhow::ensure!(
                        replayed.checkpoint_signers.iter().all(|named| *named == signer),
                        "checkpoint signer differs from --signer"
                    );
                }
                replayed
            };
            if let Some(attestation) = attestation {
                let release = ReleaseManifest::load(&release.expect("clap requires --release with --attestation"))?;
                let verified = verify(&attestation, &release, &processor, &kds, root.as_deref())?;
                ensure_signers_verified(&replayed.checkpoint_signers, &verified)?;
                println!(
                    "attestation verified: release {}, enclave {}, config {}",
                    verified.release_name, verified.enclave_address, verified.config_name
                );
            }
            let activations: Vec<String> =
                replayed.activations.iter().map(|(version, seq)| format!("{version:?} from seq {seq}")).collect();
            println!(
                "verified: {} commands, {} checkpoints, final root 0x{}, protocol V2{}, client auth {}, operator {}",
                replayed.commands,
                replayed.checkpoints,
                hex::encode(replayed.engine.state_root()),
                activations.iter().map(|a| format!(", {a}")).collect::<String>(),
                match replayed.engine.client_auth() {
                    None => "none recorded".to_owned(),
                    Some(ClientAuth::Unsigned) => "unsigned".to_owned(),
                    Some(ClientAuth::Eip712 { chain_id, vault }) => format!("eip712 (chain {chain_id}, vault {vault})"),
                },
                match replayed.engine.operator() {
                    None => "none recorded".to_owned(),
                    Some(Operator { address, chain_id, vault }) =>
                        format!("{address} (chain {chain_id}, vault {vault})"),
                }
            );
            println!(
                "checkpoint signers: {}",
                replayed.checkpoint_signers.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            );
            let pending = replayed.unsigned.len() + replayed.deferred.len();
            if pending > 0 {
                println!("note: {} approved withdrawal(s) await signature release", pending);
            }
            Ok(())
        }
        Cmd::VerifyAttestation { attestation, release, processor, kds, root } => {
            init_logging("info")?;
            let release = ReleaseManifest::load(&release)?;
            let verified = verify(&attestation, &release, &processor, &kds, root.as_deref())?;
            println!(
                "verified: release {}, enclave {}, config {}",
                verified.release_name, verified.enclave_address, verified.config_name
            );
            Ok(())
        }
        Cmd::Admin(args) => {
            let env = KeyEnv {
                key_file: std::env::var_os("OPERATOR_KEY_FILE").map(PathBuf::from),
                key: std::env::var("OPERATOR_KEY").ok().map(HexKey),
            };
            tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(admin::run(args, env)).map(drop)
        }
    }
}

/// Verifies the attestation file against the release manifest. See `verify_attestation`'s error
/// cases; the runtime exists for the collateral fetches.
fn verify(
    attestation: &std::path::Path,
    release: &ReleaseManifest,
    processor: &Option<String>,
    kds: &str,
    root: Option<&std::path::Path>,
) -> anyhow::Result<VerifiedAttestation> {
    let processor = processor.as_deref().context("--processor is required to verify a quote")?;
    let root_pem = root.map(std::fs::read_to_string).transpose()?;
    let options = VerifyOptions { processor, kds_base: kds, root_pem: root_pem.as_deref() };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("building the subcommand runtime")
        .block_on(verify_attestation(&load_attestation(attestation)?, release, &options))
}

#[cfg(test)]
mod replay_cli_tests {
    use super::*;

    #[test]
    fn custody_audit_requires_independent_signer_and_owns_the_domain() {
        assert!(
            Cli::try_parse_from(["engine", "replay", "--log", "public", "--custody-config", "audit.toml"]).is_err()
        );
        let signer = "0x1111111111111111111111111111111111111111";
        assert!(
            Cli::try_parse_from([
                "engine",
                "replay",
                "--log",
                "public",
                "--custody-config",
                "audit.toml",
                "--signer",
                signer
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "engine",
                "replay",
                "--log",
                "public",
                "--custody-config",
                "audit.toml",
                "--signer",
                signer,
                "--chain-id",
                "31337",
                "--vault",
                signer
            ])
            .is_err()
        );
    }

    #[test]
    fn strict_replay_without_custody_config_keeps_existing_arguments() {
        let cli = Cli::try_parse_from(["engine", "replay", "--log", "public"]).unwrap();
        assert!(matches!(cli.cmd, Cmd::Replay { custody_config: None, signer: None, .. }));
        let signer = "0x1111111111111111111111111111111111111111";
        let cli = Cli::try_parse_from([
            "engine",
            "replay",
            "--log",
            "public",
            "--chain-id",
            "31337",
            "--vault",
            signer,
            "--signer",
            signer,
        ])
        .unwrap();
        assert!(matches!(cli.cmd, Cmd::Replay { custody_config: None, chain_id: Some(31337), signer: Some(_), .. }));
    }

}
