//! Operator registry logs decode, beside the staking contract's, only from the configured registry.
use super::*;
use crate::custody::evm::StakingEventKind;
use crate::custody::registry::{Elected, RotationChanged};

const STAKING: Address = Address::repeat_byte(0x5a);
const REGISTRY: Address = Address::repeat_byte(0x6b);

/// A reader with the staking contract configured, and the registry when `registry`.
fn reader(registry: bool) -> VaultRpc {
    let mut cfg: EvmCustodySection = toml::from_str(&evm_toml(&std::env::temp_dir(), 1_000)).unwrap();
    let section = format!("contract = \"{STAKING}\"\ncursor_path = \"unused-staking-cursor\"");
    let section = if registry { format!("{section}\n[registry]\ncontract = \"{REGISTRY}\"") } else { section };
    cfg.staking = Some(toml::from_str(&section).unwrap());
    VaultRpc::new(&cfg, &cfg.providers[0].rpc_url, VAULT).unwrap()
}

/// `data` emitted by `address`, in block 7 at log index 2.
fn emitted(address: Address, data: LogData) -> Log {
    let mut log = log(data);
    log.inner.address = address;
    log
}

#[test]
fn registry_logs_decode_to_mirrorable_events_with_effective_times_in_milliseconds() {
    let rpc = reader(true);
    let switch = RotationChanged { enabled: false, electionCancelled: true }.encode_log_data();
    let event = rpc.decode_staking(&emitted(REGISTRY, switch)).unwrap();
    assert_eq!(event.kind, StakingEventKind::RotationChanged { enabled: false, election_cancelled: true });
    assert_eq!((event.block, event.log_index), (7, 2));
    let elected = Elected {
        epoch: 4,
        candidate: Address::repeat_byte(0xc1),
        adminKey: Address::repeat_byte(0xa1),
        release: B256::repeat_byte(3),
        effectiveAt: 1_793_000_000,
    };
    let event = rpc.decode_staking(&emitted(REGISTRY, elected.encode_log_data())).unwrap();
    assert_eq!(
        event.kind,
        StakingEventKind::Elected {
            epoch: 4,
            candidate: AccountId([0xc1; 20]),
            admin_key: AccountId([0xa1; 20]),
            effective_at: 1_793_000_000_000,
        }
    );
}

#[test]
fn registry_logs_are_refused_from_another_address_or_past_the_millisecond_range() {
    let elected = |effective_at| Elected {
        epoch: 1,
        candidate: Address::repeat_byte(0xc1),
        adminKey: Address::repeat_byte(0xa1),
        release: B256::ZERO,
        effectiveAt: effective_at,
    };
    let too_late = emitted(REGISTRY, elected(u64::MAX).encode_log_data());
    assert!(reader(true).decode_staking(&too_late).unwrap_err().to_string().contains("out of range"));
    // The staking contract does not emit registry events, and without a configured registry its
    // logs come from no mirrored contract.
    let from_staking = emitted(STAKING, elected(1).encode_log_data());
    assert!(reader(true).decode_staking(&from_staking).unwrap_err().to_string().contains("unexpected event"));
    let unconfigured = emitted(REGISTRY, elected(1).encode_log_data());
    assert!(reader(false).decode_staking(&unconfigured).unwrap_err().to_string().contains("unexpected log"));
}
