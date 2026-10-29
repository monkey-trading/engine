//! The command-level regression for the replay dispatch's attested-signer trust: `engine replay
//! --attestation` on a log with signed withdrawals must verify without `--signer`, taking the
//! trusted signer from the verified attestation, and must reject a `--signer` pin that disagrees
//! with it. The log and the attestation are built here, and the loopback key-distribution
//! stand-in serves the quote's collateral while the actual binary runs.

use std::path::Path;
use std::process::Output;

use anyhow::{Context as _, ensure};
use engine_core::Engine;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use engine_verify::fixtures::{
    LAUNCH_DIGEST, PROCESSOR, REPORTED_TCB, attestation, forged_report, pem, release_toml, test_chain,
};

use super::stub::kds_stub;
use crate::log::{Checkpoint, CheckpointDomain, CheckpointParent, CommandEntry, LogEntry};
use crate::sequencer::SignedWithdrawal;
use crate::tee::{AttestationBundle, EnclaveKey};
use crate::test_support::{TRADER, deposit, scratch_dir, withdraw};
use crate::withdrawals::{Authorization, LIFETIME_SECONDS};

/// The dev scenario: a hand-built log whose checkpoints and signed withdrawal one enclave key
/// signed replays through the `engine replay` binary with `--attestation` and no `--signer`, and
/// the same command with a mismatched `--signer` fails. Runs without hardware or vendor access;
/// `scripts/dev-attestation-verifier.sh` runs it, and CI runs the script. The scenario loads its
/// attestation and manifest from files, the same path the CLI takes.
#[tokio::test]
#[ignore = "dev scenario, run by scripts/dev-attestation-verifier.sh"]
async fn dev_scenario_replay_dispatch_trusts_the_attested_signer() -> anyhow::Result<()> {
    let key = EnclaveKey::generate();
    let signer = AccountId(key.address());
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([9; 20]) };
    let mut engine = Engine::new();
    let mut entries = Vec::new();
    for cmd in [
        Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)),
        Command::Admin(AdminCommand::SetOperator(Operator {
            address: AccountId([8; 20]),
            chain_id: domain.chain_id,
            vault: domain.vault,
        })),
        Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V22 }),
        deposit(100, "replay-dispatch-deposit"),
        withdraw(40),
    ] {
        push_command(&mut entries, &mut engine, cmd);
    }
    let first = checkpoint(&mut engine, &key, domain, CheckpointParent::default());
    entries.push(LogEntry::Checkpoint(first));
    entries.push(LogEntry::Withdrawal(payout(&engine, &key, domain, 1)?));

    // The attestation binds the key's address, for a configuration the release lists.
    let bundle = AttestationBundle {
        enclave_address: key.address(),
        config_sha256: [0xc0; 32],
        tls_public_key_sha256: [0x71; 32],
    };
    let chip_id = [0x9d; 64];
    let chain = test_chain(chip_id, REPORTED_TCB);
    let stub = kds_stub(&chain).await;
    let quote = forged_report(&chain, LAUNCH_DIGEST, bundle.report_data(), chip_id);
    let attestation = attestation(&bundle, &quote);

    let dir = scratch_dir("replay-dispatch-attested-signer");
    std::fs::create_dir_all(&dir)?;
    let log_path = dir.join("public.jsonl");
    let attestation_path = dir.join("attestation.json");
    let release_path = dir.join("release.toml");
    let root_path = dir.join("root.pem");
    let text = entries
        .iter()
        .map(|entry| serde_json::to_string(entry).expect("a log entry serializes") + "\n")
        .collect::<String>();
    std::fs::write(&log_path, text)?;
    std::fs::write(&attestation_path, serde_json::to_string(&attestation)?)?;
    std::fs::write(&release_path, release_toml())?;
    std::fs::write(&root_path, pem(&chain.ark))?;

    // The library path replays the same log with the key as the independently trusted signer.
    let replayed = crate::replay_path_with_trust(&log_path, Some(domain), Some(signer))?;
    assert_eq!((replayed.commands, replayed.checkpoints), (5, 1));

    let binary = engine_binary()?;

    // Without `--signer` the verified attestation's enclave address is the trusted signer, and
    // the log verifies and names it.
    let verified =
        replay_command(&binary, &log_path, &attestation_path, &release_path, &stub.base, &root_path, &[]).await?;
    let stdout = String::from_utf8_lossy(&verified.stdout);
    ensure!(
        verified.status.success()
            && stdout.contains("attestation verified: release test-release-v1")
            && stdout.contains(&key.address_hex()),
        "replay without --signer must verify against the attested signer: status {:?}, stdout {stdout}, stderr {}",
        verified.status,
        String::from_utf8_lossy(&verified.stderr)
    );

    // A `--signer` pin that is not the attested address is refused.
    let mismatched = replay_command(
        &binary,
        &log_path,
        &attestation_path,
        &release_path,
        &stub.base,
        &root_path,
        &["--signer", "0x4242424242424242424242424242424242424242"],
    )
    .await?;
    let stderr = String::from_utf8_lossy(&mismatched.stderr);
    ensure!(
        !mismatched.status.success() && stderr.contains("differs from the attested enclave address"),
        "a mismatched --signer must fail: status {:?}, stderr {stderr}",
        mismatched.status
    );
    Ok(())
}

/// Locates the built `engine` binary next to this test executable (`target/<profile>/engine`).
/// Error cases:
/// - the path cannot be derived or the binary was not built
pub(super) fn engine_binary() -> anyhow::Result<std::path::PathBuf> {
    let exe = std::env::current_exe().context("the test executable path")?;
    let binary = exe
        .parent()
        .and_then(Path::parent)
        .map(|dir| dir.join("engine"))
        .context("deriving the target directory from the test executable")?;
    ensure!(
        binary.is_file(),
        "the engine binary was not found at {}; build it first (scripts/dev-attestation-verifier.sh does)",
        binary.display()
    );
    Ok(binary)
}

/// Runs `engine replay` on `log` with the attestation inputs, plus `extra` arguments, while the
/// key-distribution stand-in answers. Awaits the child so the single-threaded runtime keeps
/// serving the stand-in.
/// Error cases:
/// - the binary cannot be spawned or awaited
async fn replay_command(
    binary: &Path,
    log: &Path,
    attestation: &Path,
    release: &Path,
    kds: &str,
    root: &Path,
    extra: &[&str],
) -> anyhow::Result<Output> {
    let output = tokio::process::Command::new(binary)
        .arg("replay")
        .arg("--log")
        .arg(log)
        .arg("--attestation")
        .arg(attestation)
        .arg("--release")
        .arg(release)
        .arg("--processor")
        .arg(PROCESSOR)
        .arg("--kds")
        .arg(kds)
        .arg("--root")
        .arg(root)
        .args(extra)
        .output()
        .await
        .context("running engine replay")?;
    Ok(output)
}

/// Applies one command to the engine and logs it with its events.
fn push_command(entries: &mut Vec<LogEntry>, engine: &mut Engine, cmd: Command) {
    let sequenced = Sequenced { seq: engine.last_seq().map_or(0, |seq| seq + 1), ts: 1_000, cmd };
    let events = engine.apply(&sequenced).expect("the fixture command applies");
    assert!(
        !events.iter().any(|event| matches!(event, Event::AdminRejected { .. } | Event::WithdrawalRejected { .. }))
    );
    entries.push(LogEntry::Command(CommandEntry { seal: None, sequenced, events: Some(events) }));
}

/// The checkpoint after the last applied command, signed by `key` in `domain`.
fn checkpoint(engine: &mut Engine, key: &EnclaveKey, domain: CheckpointDomain, parent: CheckpointParent) -> Checkpoint {
    let seq = engine.last_seq().expect("a command precedes the checkpoint");
    let root = engine.state_root();
    Checkpoint {
        parent: Some(parent),
        seq,
        root: format!("0x{}", hex::encode(root)),
        signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, seq, &root, parent),
        signer: key.address_hex(),
        domain: Some(domain),
        deferred_withdrawals: true,
    }
}

/// The signed withdrawal entry completing the approved withdrawal, with an authorization proof
/// against the checkpoint in the log.
/// Error cases:
/// - the key cannot sign the payout
fn payout(
    engine: &Engine,
    key: &EnclaveKey,
    domain: CheckpointDomain,
    signer_epoch: u64,
) -> anyhow::Result<SignedWithdrawal> {
    let proof = engine.approval_proof(TRADER, 1).context("the withdrawal was approved")?;
    let mut withdrawal = SignedWithdrawal {
        account: TRADER,
        amount: proof.approval.amount,
        nonce: 1,
        chain_id: domain.chain_id,
        vault: domain.vault,
        signature: String::new(),
        approved_seq: Some(proof.approval.approved_seq),
        authorization: Some(Authorization {
            signer: AccountId(key.address()),
            signer_epoch,
            issued_at: 1,
            expires_at: 1 + LIFETIME_SECONDS,
            checkpoint_seq: proof.seq,
            checkpoint_root: proof.root,
            siblings: proof.siblings,
        }),
    };
    withdrawal.signature = key.sign_payout(&withdrawal)?;
    Ok(withdrawal)
}
