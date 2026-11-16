//! A fixture log tail, as the archive bootstrap leaves a copy.

use super::*;

/// Writes a one-segment log running `0..=seq`, signed by `key`, and returns its head: the
/// commands apply to a real engine, whose state root the fixture's final checkpoint carries,
/// signed as real checkpoints are, so the handover's replay checks pass against fixture logs as
/// they do against real ones.
pub(super) fn write_log_tail(dir: &std::path::Path, seq: u64, key: &crate::tee::EnclaveKey) -> Head {
    write_log_through(dir, &[seq], key)
}

/// As `write_log_tail`, with a checkpoint after each of `checkpoints`, ascending, the last the head: a
/// log continued past an earlier head keeps every entry through it as it was.
pub(super) fn write_log_through(dir: &std::path::Path, checkpoints: &[u64], key: &crate::tee::EnclaveKey) -> Head {
    std::fs::create_dir_all(dir).unwrap();
    let file = format!("{:020}.jsonl", 0);
    let mut text = String::new();
    let mut engine = engine_core::Engine::new();
    let (seq, mut root) = (*checkpoints.last().expect("a head"), engine.state_root());
    for n in 0..=seq {
        let mut entry = CommandEntry {
            sequenced: Sequenced { seq: n, ts: 1_000 + n as i64, cmd: Command::Tick },
            events: Some(Vec::new()),
            seal: None,
        };
        entry.events = Some(engine.apply(&entry.sequenced).unwrap());
        text.push_str(&serde_json::to_string(&LogEntry::Command(entry)).unwrap());
        text.push('\n');
        if !checkpoints.contains(&n) {
            continue;
        }
        root = engine.state_root();
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
        let checkpoint: Checkpoint = serde_json::from_value(serde_json::json!({
            "seq": n,
            "root": format!("0x{}", hex::encode(root)),
            "signature": key.sign_checkpoint(domain.chain_id, &domain.vault.0, n, &root),
            "signer": format!("{:#x}", alloy::primitives::Address::from(key.address())),
            "domain": { "chain_id": domain.chain_id, "vault": format!("{:#x}", alloy::primitives::Address::from(domain.vault.0)) },
        }))
        .unwrap();
        text.push_str(&serde_json::to_string(&LogEntry::Checkpoint(checkpoint)).unwrap());
        text.push('\n');
    }
    let bytes = text.as_bytes();
    let listed = crate::log::SegmentInfo {
        file,
        first_seq: 0,
        last_seq: seq,
        bytes: bytes.len() as u64,
        sha256: hex::encode(sha2::Sha256::digest(bytes)),
    };
    std::fs::write(dir.join(&listed.file), bytes).unwrap();
    std::fs::write(dir.join(crate::log::MANIFEST), format!("{}\n", serde_json::to_string(&listed).unwrap())).unwrap();
    Head { seq, root: format!("0x{}", hex::encode(root)) }
}
