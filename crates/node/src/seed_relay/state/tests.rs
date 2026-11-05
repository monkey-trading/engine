use super::*;
use crate::seed_relay::ssz::is_valid_merkle_branch;

const FULU_VERSION: [u8; 4] = [6, 0, 0, 0];

#[test]
fn a_proof_from_a_state_verifies_at_the_indices_the_relay_checks() {
    let epoch = 500_000;
    let state = synthetic_state((epoch + 3) * SLOTS_PER_EPOCH + 5, FULU_VERSION, dev_mix);
    let proof = prove_mix(&state, epoch).unwrap();
    assert_eq!(proof.mix.0, dev_mix(epoch));
    assert_eq!(proof.fork_version, FULU_VERSION);
    assert_eq!((proof.fork_proof.len(), proof.mix_proof.len()), (8, 22));
    let fork_index = (FORK_FIELD << FORK_DEPTH) | 1;
    let mut version = [0u8; 32];
    version[..4].copy_from_slice(&FULU_VERSION);
    assert!(is_valid_merkle_branch(version, &proof.fork_proof, fork_index, proof.state_root.0));
    let mix_index = (RANDAO_MIXES_FIELD << MIXES_DEPTH) | (epoch % EPOCHS_PER_HISTORICAL_VECTOR) as usize;
    assert!(is_valid_merkle_branch(proof.mix.0, &proof.mix_proof, mix_index, proof.state_root.0));
    // Another epoch's mix sits elsewhere: the branch for this one does not prove it.
    assert!(!is_valid_merkle_branch(dev_mix(epoch - 1), &proof.mix_proof, mix_index, proof.state_root.0));
    assert!(!is_valid_merkle_branch(proof.mix.0, &proof.mix_proof, mix_index - 1, proof.state_root.0));
}

#[test]
fn a_state_whose_epoch_has_not_ended_or_has_reused_its_index_is_refused() {
    let epoch = 500_000;
    let last_slot = (epoch + 1) * SLOTS_PER_EPOCH - 1;
    let early = prove_mix(&synthetic_state(last_slot, FULU_VERSION, dev_mix), epoch).unwrap_err();
    assert!(early.to_string().contains("not past epoch"), "{early:#}");
    prove_mix(&synthetic_state(last_slot + 1, FULU_VERSION, dev_mix), epoch).unwrap();
    let reuse = (epoch + EPOCHS_PER_HISTORICAL_VECTOR) * SLOTS_PER_EPOCH;
    let late = prove_mix(&synthetic_state(reuse, FULU_VERSION, dev_mix), epoch).unwrap_err();
    assert!(late.to_string().contains("reused"), "{late:#}");
    prove_mix(&synthetic_state(reuse - 1, FULU_VERSION, dev_mix), epoch).unwrap();
}

#[test]
fn a_cut_or_misordered_state_is_refused() {
    let epoch = 500_000;
    let state = synthetic_state((epoch + 1) * SLOTS_PER_EPOCH, FULU_VERSION, dev_mix);
    let fixed: usize = FULU.iter().map(|field| field.fixed_size().unwrap_or(4)).sum();
    assert!(prove_mix(&state[..fixed - 1], epoch).unwrap_err().to_string().contains("fixed part"));
    // The payload header is the one variable field with content, so cutting the end cuts it.
    assert!(prove_mix(&state[..state.len() - 1], epoch).is_err());
    // The first offset must point at the end of the fixed part.
    let first_offset: usize = FULU[..7].iter().map(|field| field.fixed_size().unwrap()).sum();
    let mut shifted = state.clone();
    shifted[first_offset..first_offset + 4].copy_from_slice(&(fixed as u32 + 1).to_le_bytes());
    assert!(prove_mix(&shifted, epoch).unwrap_err().to_string().contains("offsets"));
    // Trailing bytes after the last list make it a list of partial elements.
    let mut padded = state;
    padded.push(0);
    assert!(prove_mix(&padded, epoch).unwrap_err().to_string().contains("16-byte elements"));
}

/// Fields of mainnet's state at slot 15583232, whose root matched its block's state root, each with
/// the root that state gave it: every list and container type the state holds non-empty there, and
/// the first validators and pending deposits as elements.
#[test]
fn mainnet_fields_hash_to_the_roots_their_state_gave_them() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/seed-relay-state-fields.json")).unwrap();
    let decode = |value: &serde_json::Value| hex::decode(value.as_str().unwrap().trim_start_matches("0x")).unwrap();
    let fields = fixture["fields"].as_array().unwrap();
    assert!(fields.len() >= 12);
    for field in fields {
        let index = field["index"].as_u64().unwrap() as usize;
        assert_eq!(
            FULU[index].root(&decode(&field["bytes"])).unwrap().to_vec(),
            decode(&field["root"]),
            "field {index}"
        );
    }
    for (name, sizes) in [("validators", &VALIDATOR[..]), ("pendingDeposits", &PENDING_DEPOSIT[..])] {
        for element in fixture[name].as_array().unwrap() {
            assert_eq!(container_root(&decode(&element["bytes"]), sizes).unwrap().to_vec(), decode(&element["root"]));
        }
    }
}

/// Records that fixture from a downloaded state, refusing one whose root is not the expected one:
/// `SEED_RELAY_STATE=<state.ssz> SEED_RELAY_STATE_ROOT=<0x…> cargo test -p engine-node --lib
/// seed_relay::state::tests::record_mainnet_fields -- --ignored --nocapture > fields.json`.
#[test]
#[ignore = "reads a downloaded mainnet state; records tests/fixtures/seed-relay-state-fields.json"]
fn record_mainnet_fields() {
    let state = std::fs::read(std::env::var("SEED_RELAY_STATE").unwrap()).unwrap();
    let expected = std::env::var("SEED_RELAY_STATE_ROOT").unwrap();
    let fields = split(&state).unwrap();
    let roots: Vec<[u8; 32]> = FULU.iter().zip(&fields).map(|(field, bytes)| field.root(bytes).unwrap()).collect();
    let (state_root, _) = crate::seed_relay::ssz::branch(roots.clone(), STATE_DEPTH, 0).unwrap();
    assert_eq!(format!("0x{}", hex::encode(state_root)), expected, "the state's root is not the block's");
    let hex = |bytes: &[u8]| format!("0x{}", hex::encode(bytes));
    let recorded: Vec<_> = [3, 4, 8, 9, 17, 18, 19, 20, 22, 24, 35, 36, 37]
        .into_iter()
        .map(|index| serde_json::json!({"index": index, "bytes": hex(fields[index]), "root": hex(&roots[index])}))
        .collect();
    let elements = |index: usize, sizes: &[usize]| -> Vec<serde_json::Value> {
        let size: usize = sizes.iter().sum();
        fields[index]
            .chunks(size)
            .take(2)
            .map(|element| serde_json::json!({"bytes": hex(element), "root": hex(&container_root(element, sizes).unwrap())}))
            .collect()
    };
    let fixture = serde_json::json!({
        "slot": u64::from_le_bytes(fields[SLOT_FIELD].try_into().unwrap()),
        "stateRoot": expected,
        "fields": recorded,
        "validators": elements(11, &VALIDATOR),
        "pendingDeposits": elements(34, &PENDING_DEPOSIT),
    });
    println!("{}", serde_json::to_string_pretty(&fixture).unwrap());
}

// ── Helpers ──

/// The mix the dev scenario's stand-in gives epoch `epoch`.
fn dev_mix(epoch: u64) -> [u8; 32] {
    crate::seed_relay::ssz::hash(&[0xd5; 32], &{
        let mut chunk = [0u8; 32];
        chunk[..8].copy_from_slice(&epoch.to_le_bytes());
        chunk
    })
}
