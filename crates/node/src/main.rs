//! `engine`: runs the node, replays a published log and verifies its checkpoints, builds the proofs
//! an account exits the vault with from a published log, verifies an attestation against a release
//! or a response or stream connection the attested enclave signed, or signs and submits an
//! operator's admin command.

use std::num::NonZeroUsize;
use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Args, Parser, Subcommand};
use engine_core::auth::{ClientAuth, Operator};
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use engine_node::admin::{self, AdminArgs, HexKey, KeyEnv};
use engine_node::carry::CarryArgs;
use engine_node::ct_watch::{self, CtWatchArgs};
use engine_node::exit::{ExitSubject, exit_bundle};
use engine_node::log::{CheckpointDomain, ReplayWorkers, SealChecks};
use engine_node::{
    ExpectedSigner, KDS_BASE_URL, KeyStart, NodeConfig, PublishedLog, PublishedSnapshots, SentRequest,
    VerifiedAttestation, VerifyOptions, audit, ensure_signers_verified, init_logging, init_telemetry,
    load_relayed_collateral, parse_challenge_arg, parse_launch_digest, run_dedicated, verify_attestation_files,
    verify_frames_file, verify_response_files,
};

#[derive(Subcommand)]
enum Cmd {
    /// Run the node.
    Run {
        #[arg(short, long, env = "CONFIG_PATH", default_value = "config/dev.toml")]
        config: PathBuf,
        /// Seal new enclave keys for a release that has none sealed. The vault must then register
        /// the new signer, so use it only for a first deployment or after a compromise.
        #[arg(short, long, conflicts_with = "predecessor")]
        new_key: bool,
        /// Take a release's keys from the running release whose `[tee.sealing.handover]` listener
        /// is at this address, once the vault makes this release current.
        #[arg(short, long, value_name = "HOST:PORT")]
        predecessor: Option<String>,
        /// The mock TEE's stand-in for this release's launch measurement (48 bytes of hex), from
        /// which its public stand-in for sealing derives the key. Development only.
        #[arg(long, value_name = "HEX", value_parser = parse_launch_digest)]
        mock_launch_digest: Option<[u8; 48]>,
        /// Run as the standby for the operator in effect: follow its stream into a local copy of
        /// the published log and take over when the registry's liveness rule promotes this node's
        /// candidate (needs `[standby]`). The standby holds no keys; the key flags do not apply, but
        /// `--mock-launch-digest` still names the release its delivered secrets are sealed for.
        #[arg(long, conflicts_with_all = ["new_key", "predecessor"])]
        standby: bool,
        /// Admit no client request, for a log's last start before a new log carries its state: prices,
        /// the chain's mirrors and checkpoints go on.
        #[arg(long, conflicts_with = "standby")]
        frozen: bool,
    },
    /// Replay a published log and verify every checkpoint root and signature. `--log` is a single log file, a
    /// directory of segments with their manifest (a node's log directory or a downloaded archive), an archive's
    /// URL, a node's public origin, downloaded first, with each segment the node no longer holds read from the
    /// bucket it redirects to, or a log's URL in an archive's bucket, `<public origin>/log/<chain id>/<vault>/`,
    /// read from the bucket alone through the vault's checkpoint that `--seq` and `--state-root` name. A missing
    /// log, or one with no command or no checkpoint, fails. Every
    /// instruction's chained digest is checked, every seal's form, and in full the first and last seal of each run
    /// of one signer's seals, the last authenticating every instruction in the run; `--every-seal` checks every
    /// seal in full.
    Replay {
        #[arg(short, long)]
        log: PublishedLog,
        /// With a log in an archive's bucket for `--log`: the vault's checkpoint to read it through, its `latestSeq`.
        #[arg(long, requires = "state_root")]
        seq: Option<u64>,
        /// With `--seq`: that checkpoint's root, the vault's `latestRoot`, which names its tail in the bucket.
        #[arg(long, requires = "seq")]
        state_root: Option<B256>,
        /// Explicit domain for signed logs written before domain recording.
        #[arg(long, requires = "vault")]
        chain_id: Option<u64>,
        #[arg(long, requires = "chain_id")]
        vault: Option<AccountId>,
        /// Pin an enclave signer obtained from a verified attestation or the vault. A log holding
        /// signed withdrawals needs it, or `--attestation`, to authenticate them.
        #[arg(long)]
        signer: Option<AccountId>,
        /// Verify historical signers against this deployment's finalized vault history.
        #[arg(long, requires = "signer", conflicts_with_all = ["chain_id", "vault", "attestation"])]
        custody_config: Option<PathBuf>,
        /// Verify a checkpoint signer against a release manifest; repeat for a log whose signer
        /// changes, one attestation per signer, each with its own `--release` in the same order.
        #[arg(long, requires = "release")]
        attestation: Vec<PathBuf>,
        /// The release manifest each `--attestation` verifies against, in the same order.
        #[arg(long, requires = "attestation")]
        release: Vec<PathBuf>,
        /// The vendor's product name for the certificate chain: Milan, or Genoa for a Genoa, Bergamo
        /// or Siena processor (Turin is not supported yet). The quote's VCEK must name it.
        #[arg(long, requires = "attestation")]
        processor: Option<String>,
        /// The vendor key-distribution service's base URL; the vendor's service unless overridden.
        #[arg(long, default_value_t = KDS_BASE_URL.to_owned())]
        kds: String,
        /// An extra root certificate, ASCII PEM, pinning the chain in addition to the compiled-in
        /// vendor roots. Test and private chains verify against this; release chains do not need it.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Check every instruction's seal in full, as a copy of the log does, not only each run's first and last.
        #[arg(long)]
        every_seal: bool,
        /// Threads that check command seals ahead of the replay with `--every-seal`; one per core when unset.
        #[arg(long, requires = "every_seal")]
        replay_workers: Option<NonZeroUsize>,
        /// A directory of daily snapshots, as `/v1/archive/snapshots/` serves them, or a node's URL, whose listed
        /// snapshots are downloaded first, from the node or the bucket it redirects to: the log, which must be a
        /// directory, is replayed in ranges from the checkpoints they name, each snapshot checked against its
        /// checkpoint's signed root, several ranges at once.
        #[arg(long, conflicts_with = "every_seal")]
        snapshots: Option<PublishedSnapshots>,
        /// How many ranges replay at once with `--snapshots`; one per two physical cores when unset.
        #[arg(long, requires = "snapshots")]
        jobs: Option<NonZeroUsize>,
        /// The published log a carried genesis took its state from, a path or an archive's URL as `--log`: its
        /// checkpoint at the carried seq must hold the carried root, signed, and the carried values must be its
        /// state there.
        #[arg(long)]
        carry_source: Option<PublishedLog>,
        /// A node config whose `[archive]` names the log's bucket: the segments a `--log` or `--carry-source`
        /// directory lacks, as a node's local window removes them, are put back into it from there first.
        #[arg(long)]
        archive_config: Option<PathBuf>,
    },
    /// Build the carry file a new log's genesis takes MONKEY entitlements, stakes and awards from, from a source
    /// log's published checkpoint, and print what it carries and its SHA-256.
    Carry(CarryArgs),
    /// Build the proofs an account, or the house, exits the vault with, or the receipt the root holds
    /// at a vault deposit id, from a published log alone, against the root of its checkpoint at
    /// `--seq`, which must be the vault's `latestSeq`. Prints
    /// them as JSON with each vault call ABI-encoded, to send in the order listed, skipping a call
    /// the vault refuses for the reason its description names. The log's commands must reproduce
    /// that checkpoint's root; signatures are not checked, since the vault checks every proof against
    /// the root it accepted.
    ExitProof {
        /// The published log, as `engine replay --log` takes it.
        #[arg(short, long)]
        log: PublishedLog,
        /// The checkpoint the vault last posted.
        #[arg(long)]
        seq: u64,
        /// With a log in an archive's bucket for `--log`: the vault's `latestRoot`, which names its tail there.
        #[arg(long)]
        state_root: Option<B256>,
        /// The account whose proofs to build.
        #[arg(long, conflicts_with_all = ["house", "deposit"], required_unless_present_any = ["house", "deposit"])]
        account: Option<AccountId>,
        /// Build the refunds of the house's pending deposits instead.
        #[arg(long, conflicts_with = "deposit")]
        house: bool,
        /// Build the refund of the vault's deposit at this id instead, through the receipt the root
        /// holds there: paid while claims register when the vault's records do not match it, and from
        /// the pending reserve for a pending receipt they match.
        #[arg(long)]
        deposit: Option<u64>,
    },
    /// Verify an attestation served by `/v1/attestation` against a release manifest: the quote's
    /// certificate chain up to a pinned vendor root, the launch measurement, guest policy, and
    /// reported TCB against the release, and the report data's binding of the enclave address and
    /// a configuration the release lists. A mock quote never verifies.
    VerifyAttestation {
        /// The attestation JSON `/v1/attestation` served, saved to a file.
        #[arg(short, long)]
        attestation: PathBuf,
        /// The release manifest of measurements, CPU models and configuration hashes.
        #[arg(short, long)]
        release: PathBuf,
        /// The vendor's product name for the certificate chain: Milan, or Genoa for a Genoa, Bergamo
        /// or Siena processor (Turin is not supported yet). The quote's VCEK must name it.
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
    /// Sign an admin command with the operator's key and submit it to a node, or deliver secrets into
    /// a node's attested enclave. The key comes from `--key-file`, or the `OPERATOR_KEY_FILE` or
    /// `OPERATOR_KEY` environment variable, never the command line.
    Admin(AdminArgs),
    /// Watch Certificate Transparency logs for a certificate for the API's domain whose key no
    /// verified attestation of the node names, and alert on it. Run it outside the enclave, on a
    /// host independent of the node's.
    CtWatch(CtWatchArgs),
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
///   command or no checkpoint, or none at the exit's seq
/// - an attestation does not verify against its release
/// - a saved response or stream connection does not verify against the attested signer
/// - an admin command cannot be signed or submitted, or the engine rejects it
/// - the CT watcher cannot start, or stops with an error
fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Run { config, new_key, predecessor, mock_launch_digest, standby, frozen } => {
            let cfg = NodeConfig::load(&config)?;
            let telemetry = init_telemetry(&cfg, |var| std::env::var(var).ok())?;
            if standby {
                tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(engine_node::standby::run(
                    cfg,
                    telemetry,
                    KeyStart { mock_launch_digest, ..Default::default() },
                ))
            } else {
                run_dedicated(cfg, telemetry, KeyStart { new_key, predecessor, mock_launch_digest, frozen })
            }
        }
        Cmd::Replay {
            log: path,
            seq,
            state_root,
            chain_id,
            vault,
            signer,
            custody_config,
            attestation,
            release,
            processor,
            kds,
            root,
            every_seal,
            replay_workers,
            snapshots,
            jobs,
            carry_source,
            archive_config,
        } => {
            init_logging("info")?;
            let workers = replay_workers.map_or_else(ReplayWorkers::available, ReplayWorkers::from);
            let seals = if every_seal { SealChecks::Every(workers) } else { SealChecks::RunEnds };
            let domain = chain_id.zip(vault).map(|(chain_id, vault)| CheckpointDomain { chain_id, vault });
            anyhow::ensure!(
                attestation.len() == release.len(),
                "--attestation and --release name different counts ({} and {}); pair them in order",
                attestation.len(),
                release.len()
            );
            anyhow::ensure!(
                attestation.len() <= 1 || signer.is_none(),
                "--signer pins one authority and cannot span the signer change; the attestations suffice"
            );
            // Verify the attestations before replaying: their enclave addresses are the trusted
            // signers for checkpoint and withdrawal entries, so a log whose checkpoints and
            // withdrawals the verified enclaves signed replays without a separate `--signer`.
            let verified = attestation
                .iter()
                .zip(&release)
                .map(|(attestation, release)| verify(attestation, release, &processor, &kds, root.as_deref(), None))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let log = path.local_filled(seq.zip(state_root), archive_config.as_deref())?;
            let snapshots = snapshots.map(|snapshots| snapshots.local(log.path())).transpose()?;
            let jobs = jobs.unwrap_or_else(engine_node::default_jobs);
            let ranges = snapshots.as_ref().map(|snapshots| (snapshots.path().to_owned(), jobs));
            let mut replayed = audit(log.path(), ranges, seals, custody_config, (signer, domain), &verified)?;
            if !verified.is_empty() {
                ensure_signers_verified(&replayed.checkpoint_signers, &verified)?;
                for attested in &verified {
                    println!(
                        "attestation verified: release {}, enclave {}, config {}",
                        attested.release_name, attested.enclave_address, attested.config_name
                    );
                }
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
            if replayed.ranges > 1 {
                println!("replayed in {} ranges from the daily snapshots", replayed.ranges);
            }
            for file in &replayed.skipped {
                println!("skipped {}: the log does not hold the checkpoint it names", file.display());
            }
            if replayed.pending > 0 {
                println!("note: {} approved withdrawal(s) await signature release", replayed.pending);
            }
            let source = carry_source.map(|source| source.local_filled(None, archive_config.as_deref())).transpose()?;
            engine_node::carry::check_replayed(
                &replayed.engine,
                log.path(),
                source.as_ref().map(|source| source.path()),
            )
        }
        Cmd::Carry(args) => {
            init_logging("error")?;
            engine_node::carry::run(args)
        }
        Cmd::ExitProof { log, seq, state_root, account, house: _, deposit } => {
            // The bundle is the output; only errors are logged.
            init_logging("error")?;
            let subject = match (account, deposit) {
                (Some(account), _) => ExitSubject::Account(account),
                (None, Some(deposit_id)) => ExitSubject::Deposit(deposit_id),
                (None, None) => ExitSubject::House,
            };
            let bundle = exit_bundle(log.local(state_root.map(|root| (seq, root)))?.path(), seq, subject)?;
            println!("{}", serde_json::to_string_pretty(&bundle)?);
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
            tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(admin::dispatch(args, env))
        }
        Cmd::CtWatch(args) => {
            tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(ct_watch::run(args))
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
mod replay_cli_tests;
