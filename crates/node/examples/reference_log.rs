//! Writes the node's reference log for a protocol version from engine-core's: each command with the
//! events it produced, a withdrawal entry for each withdrawal approved before `V22`, and a checkpoint
//! every ten commands and after the last. A `V22` payout stays unsigned: its typed authorization
//! needs a checkpoint the vault accepted. Run it once per protocol version and commit the output; it
//! refuses to overwrite a log that already exists. Protocol fixtures use placeholder enclave
//! signatures for root/event replay; the strict CLI verifier requires real signatures.
//!
//! `cargo run -p engine-node --example reference_log -- crates/core/tests/reference/v<N>.jsonl
//! crates/node/tests/reference/v<N>.jsonl`

use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::{Context, bail};
use engine_core::Engine;
use engine_core::command::Sequenced;
use engine_core::event::Event;
use engine_core::types::AccountId;
use engine_node::log::{Checkpoint, CommandEntry, LogEntry};
use engine_node::sequencer::SignedWithdrawal;

const CHECKPOINT_EVERY: usize = 10;

/// Replays engine-core's reference log and writes the node's.
/// Error cases:
/// - the arguments are not two paths
/// - the engine-core log cannot be read, or a line is not a sequenced command
/// - a command does not apply
/// - the node log already exists, or cannot be written
fn main() -> anyhow::Result<()> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let [source, out] = args.as_slice() else {
        bail!("usage: reference_log <engine-core log> <node log>");
    };
    let text = fs::read_to_string(source).with_context(|| format!("reading {}", source.display()))?;
    let lines: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .with_context(|| format!("creating {}, which must not exist yet", out.display()))?;
    let mut writer = BufWriter::new(file);
    let mut engine = Engine::new();
    for (n, line) in lines.iter().enumerate() {
        let sequenced: Sequenced =
            serde_json::from_str(line).with_context(|| format!("{} line {}", source.display(), n + 1))?;
        let events = engine.apply(&sequenced).with_context(|| format!("applying seq {}", sequenced.seq))?;
        let approved: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::WithdrawalApproved { account, amount, nonce }
                    if engine.withdrawal_approval(*account, *nonce).is_none() =>
                {
                    Some((*account, *amount, *nonce))
                }
                _ => None,
            })
            .collect();
        write(&mut writer, &LogEntry::Command(CommandEntry { seal: None, sequenced, events: Some(events) }))?;
        for (account, amount, nonce) in approved {
            let (chain_id, vault, signature) = (31337, AccountId([0; 20]), "0x".to_owned());
            write(
                &mut writer,
                &LogEntry::Withdrawal(SignedWithdrawal {
                    account,
                    amount,
                    nonce,
                    chain_id,
                    vault,
                    signature,
                    approved_seq: None,
                    authorization: None,
                }),
            )?;
        }
        if (n + 1).is_multiple_of(CHECKPOINT_EVERY) || n + 1 == lines.len() {
            let seq = engine.last_seq().context("a command was applied")?;
            let root = format!("0x{}", hex::encode(engine.state_root()));
            let (signature, signer) = ("0x".to_owned(), "0x".to_owned());
            write(
                &mut writer,
                &LogEntry::Checkpoint(Checkpoint {
                    parent: None,
                    seq,
                    root,
                    signature,
                    signer,
                    domain: None,
                    deferred_withdrawals: false,
                }),
            )?;
        }
    }
    writer.flush().with_context(|| format!("writing {}", out.display()))
}

/// Writes one entry as a JSON line.
/// Error cases:
/// - the write fails
fn write(writer: &mut BufWriter<std::fs::File>, entry: &LogEntry) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *writer, entry).context("writing log entry")?;
    writer.write_all(b"\n").context("writing log entry")
}
