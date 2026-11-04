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

// ── Helpers ──

/// The mix the dev scenario's stand-in gives epoch `epoch`.
fn dev_mix(epoch: u64) -> [u8; 32] {
    crate::seed_relay::ssz::hash(&[0xd5; 32], &{
        let mut chunk = [0u8; 32];
        chunk[..8].copy_from_slice(&epoch.to_le_bytes());
        chunk
    })
}
