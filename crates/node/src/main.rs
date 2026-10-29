//! `engine`: runs the node, replays a published log and verifies its checkpoints, verifies an
//! attestation against a release or a response or stream connection the attested enclave signed,
//! or signs and submits an operator's admin command.

use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Args, Parser, Subcommand};
use engine_core::auth::{ClientAuth, Operator};
use engine_core::types::AccountId;
use engine_node::admin::{self, AdminArgs, HexKey, KeyEnv};
use engine_node::log::CheckpointDomain;
use engine_node::{
    ExpectedSigner, KDS_BASE_URL, NodeConfig, SentRequest, VerifiedAttestation, VerifyOptions, ensure_signers_verified,
    init_logging, init_telemetry, load_relayed_collateral, parse_challenge_arg, replay_path_with_custody,
    replay_path_with_trust, run_dedicated, verify_attestation_files, verify_frames_file, verify_response_files,
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
        /// (the CPU generation's codename, for example Milan or Genoa; Turin is not supported yet).
        #[arg(long, requires = "attestation")]
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
        /// (the CPU generation's codename, for example Milan or Genoa; Turin is not supported yet).
        #[arg(long)]
        processor: String,
        /// The vendor key-distribution service's base URL; the vendor's service unless overridden.
        #[arg(long, default_value_t = KDS_BASE_URL.to_owned())]
        kds: String,
        /// An extra root certificate, ASCII PEM, pinning the chain in addition to the compiled-in
        /// vendor roots. Test and private chains verify against this; release chains do not need it.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Take the vendor collateral from a node instead of the key-distribution service: a file
        /// holding what the node's `/v1/attestation/collateral` served, or that route's URL. It is
        /// verified against the pinned roots like fetched collateral, so the node adds no trust.
        #[arg(long)]
        collateral: Option<String>,
    },
    /// Verify a response a node served, saved with `curl -D <headers> -o <body>`: its
    /// `X-Engine-Signature` over the request and its body, the challenge sent, the node's time and
    /// log head, and the status and body, by the attested enclave address. Exits non-zero on any
    /// mismatch.
    VerifyResponse {
        #[command(flatten)]
        answer: AnswerArgs,
        /// The request's method.
        #[arg(long, default_value = "GET")]
        method: String,
        /// The request's path and query, exactly as sent.
        #[arg(long)]
        path: String,
        /// The request's body, exactly as sent; none when omitted.
        #[arg(long)]
        request_body: Option<PathBuf>,
        /// The status line and headers, as `curl -D` saves them.
        #[arg(long)]
        headers: PathBuf,
        /// The body as received.
        #[arg(long)]
        body: PathBuf,
    },
    /// Verify a signed stream connection (`/v1/stream?signed=1&challenge=…`), saved one JSON frame a
    /// line from its first: each frame in its place on the connection the request opened, under the
    /// challenge sent, by the attested enclave address. Exits non-zero at the first frame that
    /// fails.
    VerifyFrames {
        #[command(flatten)]
        answer: AnswerArgs,
        /// The path and query of the request that opened the connection, exactly as sent.
        #[arg(long)]
        path: String,
        /// The saved frames.
        #[arg(long)]
        frames: PathBuf,
    },
    /// Sign an admin command with the operator's key and submit it to a node. The key comes from
    /// `--key-file`, or the `OPERATOR_KEY_FILE` or `OPERATOR_KEY` environment variable, never the
    /// command line.
    Admin(AdminArgs),
}

/// Who must have signed an answer, for which deployment, and under which challenge.
#[derive(Args)]
struct AnswerArgs {
    /// The enclave address the node's verified attestation names.
    #[arg(long)]
    signer: AccountId,
    /// The custody chain id of the deployment the node serves.
    #[arg(long)]
    chain_id: u64,
    /// The custody vault of the deployment the node serves.
    #[arg(long)]
    vault: AccountId,
    /// The 64 hex characters the client sent as its challenge; 64 zeros when it sent none.
    #[arg(long, value_parser = parse_challenge_arg)]
    challenge: [u8; 32],
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
/// - a saved response or stream connection does not verify against the attested signer
/// - an admin command cannot be signed or submitted, or the engine rejects it
fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Run { config } => {
            let cfg = NodeConfig::load(&config)?;
            let telemetry = init_telemetry(&cfg, |var| std::env::var(var).ok())?;
            run_dedicated(cfg, telemetry)
        }
        Cmd::Replay {
            log: path,
            chain_id,
            vault,
            signer,
            custody_config,
            attestation,
            release,
            processor,
            kds,
            root,
        } => {
            init_logging("info")?;
            let domain = chain_id.zip(vault).map(|(chain_id, vault)| CheckpointDomain { chain_id, vault });
            // Verify the attestation before replaying: its enclave address is the trusted signer
            // for withdrawal entries, so a log whose checkpoints and withdrawals the verified
            // enclave signed replays without a separate `--signer`.
            let verified = attestation
                .map(|attestation| {
                    let release = release.expect("clap requires --release with --attestation");
                    verify(&attestation, &release, &processor, &kds, root.as_deref(), None)
                })
                .transpose()?;
            let mut replayed = if let Some(config) = custody_config {
                let cfg = NodeConfig::load(&config)?;
                let signer = signer.ok_or_else(|| anyhow::anyhow!("--custody-config requires --signer"))?;
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(replay_path_with_custody(&path, &cfg, signer))?
            } else {
                let trusted = engine_node::reconcile_signer(signer, verified.as_ref())?;
                replay_path_with_trust(&path, domain, trusted)?
            };
            if let Some(verified) = verified {
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
        Cmd::VerifyAttestation { attestation, release, processor, kds, root, collateral } => {
            init_logging("info")?;
            let verified =
                verify(&attestation, &release, &Some(processor), &kds, root.as_deref(), collateral.as_deref())?;
            println!(
                "verified: release {}, enclave {}, config {}",
                verified.release_name, verified.enclave_address, verified.config_name
            );
            Ok(())
        }
        Cmd::VerifyResponse { answer, method, path, request_body, headers, body } => {
            let request_body = request_body.map(std::fs::read).transpose().context("reading --request-body")?;
            let request_body = request_body.as_deref().unwrap_or_default();
            let request = SentRequest { method: &method, path_and_query: &path, body: request_body };
            let verified = verify_response_files(&answer.expected(), &request, &headers, &body)?;
            println!(
                "verified: status {}, signed by {} at {} ms, log head seq {}",
                verified.status, answer.signer, verified.time_ms, verified.head_seq
            );
            Ok(())
        }
        Cmd::VerifyFrames { answer, path, frames } => {
            let count = verify_frames_file(&answer.expected(), &path, &frames)?;
            println!("verified: {count} frames, n 0 to {}, signed by {}", count - 1, answer.signer);
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

impl AnswerArgs {
    /// The signer, deployment, and challenge to verify against.
    fn expected(&self) -> ExpectedSigner {
        ExpectedSigner { signer: self.signer, chain_id: self.chain_id, vault: self.vault, challenge: self.challenge }
    }
}

/// Verifies the attestation file against the release manifest file, on a runtime for the
/// collateral fetches. See `verify_attestation`'s error cases.
fn verify(
    attestation: &std::path::Path,
    release: &std::path::Path,
    processor: &Option<String>,
    kds: &str,
    root: Option<&std::path::Path>,
    collateral: Option<&str>,
) -> anyhow::Result<VerifiedAttestation> {
    let processor = processor.as_deref().context("--processor is required with --attestation")?;
    let root_pem = root.map(std::fs::read_to_string).transpose()?;
    let options = VerifyOptions { processor, kds_base: kds, root_pem: root_pem.as_deref(), collateral: None };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("building the subcommand runtime")
        .block_on(async {
            let relayed = match collateral {
                Some(source) => Some(load_relayed_collateral(source, &options).await?),
                None => None,
            };
            let options = VerifyOptions { collateral: relayed.as_ref(), ..options };
            verify_attestation_files(attestation, release, &options).await
        })
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
