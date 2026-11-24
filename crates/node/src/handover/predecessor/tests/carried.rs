//! A fixture log whose genesis carried a source's state, in segments of a few commands.

use engine_core::command::AdminCommand;

use super::*;

/// The node's reference log whose genesis carries the reference session's state.
const REFERENCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v90-carried.jsonl");

/// Writes the reference carried log's commands into `dir`, `per_segment` to a segment, each sealed and the last
/// followed by a checkpoint, both signed by `key` for `domain`, and returns the head and the source the genesis carried from.
pub(super) fn write_carried_log(
    dir: &std::path::Path,
    per_segment: usize,
    domain: CheckpointDomain,
    key: &crate::tee::EnclaveKey,
) -> (Head, engine_core::carry::CarrySource) {
    std::fs::create_dir_all(dir).unwrap();
    let mut commands = Vec::new();
    crate::log::read_file(std::path::Path::new(REFERENCE), |entry| {
        if let LogEntry::Command(command) = entry {
            commands.push(command.sequenced);
        }
        Ok(())
    })
    .unwrap();
    // The log records the deployment it runs under, as the reference session's is not the test's.
    for sequenced in &mut commands {
        match &mut sequenced.cmd {
            Command::Admin(AdminCommand::SetClientAuth(auth)) => {
                *auth = engine_core::auth::ClientAuth::Eip712 { chain_id: domain.chain_id, vault: domain.vault };
            }
            Command::Admin(AdminCommand::SetOperator(operator)) => {
                (operator.chain_id, operator.vault) = (domain.chain_id, domain.vault);
            }
            _ => {}
        }
    }
    let mut engine = engine_core::Engine::new();
    let mut link = engine_core::eip712::B256::default();
    let mut manifest = String::new();
    let last = commands.len() - 1;
    for chunk in commands.chunks(per_segment) {
        let mut text = String::new();
        for sequenced in chunk {
            let mut entry = CommandEntry { sequenced: sequenced.clone(), events: None, seal: None };
            entry.events = Some(engine.apply(sequenced).unwrap());
            link = crate::log::command_seal::command_link(link, &entry).unwrap();
            entry.seal = Some(key.seal_command(domain, link));
            text.push_str(&serde_json::to_string(&LogEntry::Command(entry)).unwrap());
            text.push('\n');
        }
        let (first_seq, last_seq) = (chunk[0].seq, chunk[chunk.len() - 1].seq);
        if last_seq == commands[last].seq {
            let root = engine.state_root();
            let checkpoint: Checkpoint = serde_json::from_value(serde_json::json!({
                "seq": last_seq,
                "root": format!("0x{}", hex::encode(root)),
                "signature": key.sign_checkpoint(domain.chain_id, &domain.vault.0, last_seq, &root),
                "signer": format!("{:#x}", alloy::primitives::Address::from(key.address())),
                "domain": { "chain_id": domain.chain_id, "vault": format!("{:#x}", alloy::primitives::Address::from(domain.vault.0)) },
            }))
            .unwrap();
            text.push_str(&serde_json::to_string(&LogEntry::Checkpoint(checkpoint)).unwrap());
            text.push('\n');
        }
        let bytes = text.as_bytes();
        let listed = crate::log::SegmentInfo {
            file: format!("{first_seq:020}.jsonl"),
            first_seq,
            last_seq,
            bytes: bytes.len() as u64,
            sha256: hex::encode(sha2::Sha256::digest(bytes)),
        };
        std::fs::write(dir.join(&listed.file), bytes).unwrap();
        manifest.push_str(&format!("{}\n", serde_json::to_string(&listed).unwrap()));
    }
    std::fs::write(dir.join(crate::log::MANIFEST), manifest).unwrap();
    let head = Head { seq: commands[last].seq, root: format!("0x{}", hex::encode(engine.state_root())) };
    (head, engine.carried_from().expect("the reference log carried"))
}
