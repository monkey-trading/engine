//! `engine`: runs the node, replays a published log and verifies its checkpoints, or signs and
//! submits an operator's admin command.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use engine_core::auth::{ClientAuth, Operator};
use engine_core::types::AccountId;
use engine_node::admin::{self, AdminArgs, HexKey, KeyEnv};
use engine_node::log::CheckpointDomain;
use engine_node::{
    NodeConfig, init_logging, init_telemetry, replay_path_with_custody, replay_path_with_trust, run_dedicated,
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
        #[arg(long, requires = "signer", conflicts_with_all = ["chain_id", "vault"])]
        custody_config: Option<PathBuf>,
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
/// - an admin command cannot be signed or submitted, or the engine rejects it
fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Run { config } => {
            let cfg = NodeConfig::load(&config)?;
            let telemetry = init_telemetry(&cfg, |var| std::env::var(var).ok())?;
            run_dedicated(cfg, telemetry)
        }
        Cmd::Replay { log: path, chain_id, vault, signer, custody_config } => {
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
        Cmd::Admin(args) => {
            let env = KeyEnv {
                key_file: std::env::var_os("OPERATOR_KEY_FILE").map(PathBuf::from),
                key: std::env::var("OPERATOR_KEY").ok().map(HexKey),
            };
            tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(admin::run(args, env)).map(drop)
        }
    }
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
