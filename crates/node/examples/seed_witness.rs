//! Builds a `SeedRelay` witness from a beacon state and its block header, with the seed relayer's
//! own code, for `scripts/seed-relay-fixtures.sh`: `seed_witness <state.ssz> <header.json> <epoch>`,
//! where the header is a beacon API `/eth/v1/beacon/headers` answer. It prints the witness as JSON
//! and refuses a state whose root is not the header's `state_root`.

use std::path::PathBuf;

use anyhow::{Context, ensure};
use engine_node::seed_relay::BeaconBlockHeader;
use engine_node::seed_relay::state::prove_mix;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct Answer {
    data: Data,
}

#[derive(Deserialize)]
struct Data {
    header: Signed,
}

#[derive(Deserialize)]
struct Signed {
    message: BeaconBlockHeader,
}

/// Reads the state and header, proves the epoch's mix, and prints the witness.
/// Error cases:
/// - the arguments, files or epoch do not parse, the state is not a Fulu state past the epoch, or its
///   root is not the header's state root
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: seed_witness <state.ssz> <header.json> <epoch>";
    let state = PathBuf::from(args.next().context(usage)?);
    let header = PathBuf::from(args.next().context(usage)?);
    let epoch: u64 = args.next().context(usage)?.parse().context("epoch")?;
    let state = std::fs::read(&state).with_context(|| format!("reading {}", state.display()))?;
    let answer: Answer = serde_json::from_slice(&std::fs::read(&header).context("reading the header")?)?;
    let header = answer.data.header.message;
    let proof = prove_mix(&state, epoch)?;
    ensure!(
        proof.state_root == header.state_root && proof.slot == header.slot,
        "the state hashes to {} at slot {}, but the header names {} at slot {}",
        proof.state_root,
        proof.slot,
        header.state_root,
        header.slot
    );
    let witness = json!({
        "epoch": epoch,
        "beaconRoot": header.root(),
        "header": {
            "slot": header.slot,
            "proposerIndex": header.proposer_index,
            "parentRoot": header.parent_root,
            "stateRoot": header.state_root,
            "bodyRoot": header.body_root,
        },
        "forkVersion": format!("0x{}", hex::encode(proof.fork_version)),
        "forkProof": proof.fork_proof,
        "seed": proof.mix,
        "seedProof": proof.mix_proof,
    });
    println!("{}", serde_json::to_string_pretty(&witness)?);
    Ok(())
}
