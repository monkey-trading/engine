//! The vote contract's passed proposals decode, beside the schedule's events, only from the configured
//! vote contract, with the admin command their bytes are the canonical JSON of.
use super::*;
use crate::custody::evm::StakingEventKind;
use crate::custody::registry::ProposalPassed;
use engine_core::command::AdminCommand;

const STAKING: Address = Address::repeat_byte(0x5a);
const REGISTRY: Address = Address::repeat_byte(0x6b);
const SCHEDULE: Address = Address::repeat_byte(0x7c);
const VOTE: Address = Address::repeat_byte(0x7d);

/// A reader with the staking contract, its registry, the schedule and the vote contract configured.
fn reader() -> VaultRpc {
    let mut cfg: EvmCustodySection = toml::from_str(&evm_toml(&std::env::temp_dir(), 1_000)).unwrap();
    let section = format!(
        "contract = \"{STAKING}\"\ncursor_path = \"unused-staking-cursor\"\n[registry]\ncontract = \"{REGISTRY}\"\nschedule = \"{SCHEDULE}\"\n[registry.vote]\ncontract = \"{VOTE}\"\nrestriction_ttl = \"1d\""
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

/// Proposal 4 passing on the fast path with `command`, hashed to `hash` or, without one, to itself.
fn passed(command: &[u8], hash: Option<B256>) -> LogData {
    ProposalPassed {
        id: U256::from(4),
        setId: 2,
        fast: true,
        yesWeight: U256::from(70),
        totalWeight: U256::from(100),
        commandHash: hash.unwrap_or_else(|| alloy::primitives::keccak256(command)),
        command: command.to_vec().into(),
    }
    .encode_log_data()
}

#[test]
fn a_passed_proposal_decodes_with_its_canonical_command() {
    let rpc = reader();
    let command = AdminCommand::SetReduceOnly { market: Some(1), reduce_only: true };
    let bytes = serde_json::to_vec(&command).unwrap();
    let event = rpc.decode_staking(&emitted(VOTE, passed(&bytes, None))).unwrap();
    let expected = StakingEventKind::ProposalPassed {
        proposal: 4,
        fast: true,
        command_hash: alloy::primitives::keccak256(&bytes),
        command: Some(Box::new(command)),
    };
    assert_eq!(event.kind, expected);
    // The same command spaced out is not its canonical JSON: it decodes to no command.
    let spaced = br#"{"action": "set_reduce_only", "market": 1, "reduce_only": true}"#;
    let event = rpc.decode_staking(&emitted(VOTE, passed(spaced, None))).unwrap();
    assert!(matches!(event.kind, StakingEventKind::ProposalPassed { command: None, .. }), "{event:?}");
}

#[test]
fn a_command_not_hashing_to_its_event_or_a_proposal_from_another_contract_is_refused() {
    let rpc = reader();
    let bytes = serde_json::to_vec(&AdminCommand::SetReduceOnly { market: None, reduce_only: true }).unwrap();
    let err = rpc.decode_staking(&emitted(VOTE, passed(&bytes, Some(B256::repeat_byte(9))))).unwrap_err();
    assert!(err.to_string().contains("does not hash to the hash its event carries"), "{err:#}");
    let err = rpc.decode_staking(&emitted(SCHEDULE, passed(&bytes, None))).unwrap_err();
    assert!(err.to_string().contains("unexpected schedule event"), "{err:#}");
}
