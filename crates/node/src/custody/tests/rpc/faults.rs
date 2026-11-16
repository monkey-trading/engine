//! The fault proof contract's proven faults decode, beside the registry's events, only from the
//! configured fault proof contract.
use super::*;
use crate::custody::evm::StakingEventKind;
use crate::custody::registry::FaultProven;

const STAKING: Address = Address::repeat_byte(0x5a);
const REGISTRY: Address = Address::repeat_byte(0x6b);
const SCHEDULE: Address = Address::repeat_byte(0x7c);
const FAULT_PROOF: Address = Address::repeat_byte(0x7e);

/// A reader with the staking contract, its registry, the schedule and the fault proof contract configured.
fn reader() -> VaultRpc {
    let mut cfg: EvmCustodySection = toml::from_str(&evm_toml(&std::env::temp_dir(), 1_000)).unwrap();
    let section = format!(
        "contract = \"{STAKING}\"\ncursor_path = \"unused-staking-cursor\"\n[registry]\ncontract = \"{REGISTRY}\"\nschedule = \"{SCHEDULE}\"\n[registry.fault_proof]\ncontract = \"{FAULT_PROOF}\"\npenalty = {{ initial_bps = 5000, decay = \"10d\" }}"
    );
    cfg.staking = Some(toml::from_str(&section).unwrap());
    VaultRpc::new(&cfg, &cfg.providers[0].rpc_url, VAULT).unwrap()
}

/// `data` emitted by `address`, in block 7 at log index 2.
fn emitted(address: Address, data: LogData) -> Log {
    let mut log = log(data);
    log.inner.address = address;
    log
}

/// Turn 20,754's assignment at position 1, proven at seq 42 of signer epoch 3.
fn proven() -> LogData {
    let member = Address::repeat_byte(0xb2);
    FaultProven { member, turn: 20_754, position: 1, signerEpoch: 3, seq: 42 }.encode_log_data()
}

#[test]
fn a_proven_fault_decodes_with_its_member_and_assignment() {
    let event = reader().decode_staking(&emitted(FAULT_PROOF, proven())).unwrap();
    let expected = StakingEventKind::FaultProven {
        member: engine_core::types::AccountId([0xb2; 20]),
        turn: 20_754,
        position: 1,
        signer_epoch: 3,
        seq: 42,
    };
    assert_eq!(event.kind, expected);
}

#[test]
fn a_proven_fault_from_another_contract_is_refused() {
    let rpc = reader();
    let err = rpc.decode_staking(&emitted(SCHEDULE, proven())).unwrap_err();
    assert!(err.to_string().contains("unexpected schedule event"), "{err:#}");
    let err = rpc.decode_staking(&emitted(Address::repeat_byte(0x99), proven())).unwrap_err();
    assert!(err.to_string().contains("unexpected log from"), "{err:#}");
}
