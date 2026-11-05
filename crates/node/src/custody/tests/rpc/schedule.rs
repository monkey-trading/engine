//! Operator schedule logs decode, beside the registry's, only from the configured schedule.
use super::*;
use crate::custody::evm::StakingEventKind;
use crate::custody::registry::{RotationSetAdopted, TurnAssigned};
use engine_core::emissions::TokenUnits;
use engine_core::turns::{SetMember, TurnReason};

const STAKING: Address = Address::repeat_byte(0x5a);
const REGISTRY: Address = Address::repeat_byte(0x6b);
const SCHEDULE: Address = Address::repeat_byte(0x7c);

/// A reader with the staking contract, its registry and the registry's schedule configured.
fn reader() -> VaultRpc {
    let mut cfg: EvmCustodySection = toml::from_str(&evm_toml(&std::env::temp_dir(), 1_000)).unwrap();
    let section = format!(
        "contract = \"{STAKING}\"\ncursor_path = \"unused-staking-cursor\"\n[registry]\ncontract = \"{REGISTRY}\"\nschedule = \"{SCHEDULE}\""
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

fn adopted(weights: Vec<U256>) -> RotationSetAdopted {
    RotationSetAdopted {
        setId: 3,
        epoch: 9,
        effectiveAt: 1_793_000_000,
        quorum: true,
        candidates: vec![Address::repeat_byte(0xc1), Address::repeat_byte(0xc2)],
        adminKeys: vec![Address::repeat_byte(0xa1), Address::repeat_byte(0xa2)],
        weights,
    }
}

fn assigned(reason: u8) -> TurnAssigned {
    TurnAssigned {
        turn: 20_754,
        setId: 3,
        position: 1,
        reason,
        candidate: Address::repeat_byte(0xc2),
        adminKey: Address::repeat_byte(0xa2),
        release: B256::repeat_byte(5),
        generation: 11,
    }
}

#[test]
fn schedule_logs_decode_to_rotation_sets_and_turns() {
    let rpc = reader();
    let set = adopted(vec![U256::from(50), U256::from(30)]).encode_log_data();
    let event = rpc.decode_staking(&emitted(SCHEDULE, set)).unwrap();
    let member = |byte: u8, weight: u128| SetMember {
        candidate: AccountId([0xc0 + byte; 20]),
        admin_key: AccountId([0xa0 + byte; 20]),
        weight: TokenUnits(weight),
    };
    assert_eq!(
        event.kind,
        StakingEventKind::RotationSetAdopted {
            set: 3,
            epoch: 9,
            effective_at: 1_793_000_000_000,
            quorum: true,
            members: vec![member(1, 50), member(2, 30)],
        }
    );
    for (reason, expected) in [(0, TurnReason::Draw), (1, TurnReason::Skip), (2, TurnReason::Promotion)] {
        let event = rpc.decode_staking(&emitted(SCHEDULE, assigned(reason).encode_log_data())).unwrap();
        assert_eq!(
            event.kind,
            StakingEventKind::TurnAssigned {
                turn: 20_754,
                set: 3,
                position: 1,
                reason: expected,
                candidate: AccountId([0xc2; 20]),
                admin_key: AccountId([0xa2; 20]),
            }
        );
    }
}

#[test]
fn schedule_logs_are_refused_from_another_address_or_malformed() {
    let rpc = reader();
    let turn = assigned(0).encode_log_data();
    let err = rpc.decode_staking(&emitted(Address::repeat_byte(0x99), turn)).unwrap_err();
    assert!(format!("{err:#}").contains("unexpected log from"), "{err:#}");
    // The registry's address decodes registry events only.
    let err = rpc.decode_staking(&emitted(REGISTRY, assigned(0).encode_log_data())).unwrap_err();
    assert!(format!("{err:#}").contains("unexpected registry event"), "{err:#}");
    let err = rpc.decode_staking(&emitted(SCHEDULE, assigned(3).encode_log_data())).unwrap_err();
    assert!(format!("{err:#}").contains("unknown reason 3"), "{err:#}");
    let err = rpc.decode_staking(&emitted(SCHEDULE, adopted(vec![U256::from(1)]).encode_log_data())).unwrap_err();
    assert!(format!("{err:#}").contains("differ in length"), "{err:#}");
    let too_heavy = adopted(vec![U256::from(1), U256::from(u128::MAX) + U256::from(1)]).encode_log_data();
    let err = rpc.decode_staking(&emitted(SCHEDULE, too_heavy)).unwrap_err();
    assert!(format!("{err:#}").contains("exceeds u128"), "{err:#}");
}
