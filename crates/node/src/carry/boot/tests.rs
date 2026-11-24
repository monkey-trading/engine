use std::cell::Cell;
use std::fs;

use alloy::primitives::{Address, B256};
use engine_core::carry::{CarryPart, CarrySource, StateCarried};
use engine_core::emissions::TokenUnits;
use engine_core::types::AccountId;

use super::*;
use crate::test_support::scratch_dir;

const VAULT: Address = Address::repeat_byte(0x11);
const SOURCE: Address = Address::repeat_byte(0x22);
const OPERATOR: Address = Address::repeat_byte(0x33);
const ROOT: B256 = B256::repeat_byte(0x44);
const TOKEN: Address = Address::repeat_byte(0x55);
const REGISTRY: Address = Address::repeat_byte(0x77);

/// A condition a test unmeets, named by the text its sentence holds.
type Unmet = (&'static str, fn(&mut MoveState));

fn expected() -> Expected {
    Expected { chain_id: 42_161, vault: VAULT, source: SOURCE, operator: OPERATOR }
}

/// The chain once the move is done, at the source's checkpoint 70.
fn done() -> MoveState {
    MoveState {
        block: 9,
        source_signer: Address::ZERO,
        source_checkpoint: Some((70, ROOT)),
        source_registry: REGISTRY,
        registry_vault: VAULT,
        rotation_switch: false,
        admin_key: OPERATOR,
        minter_vault: VAULT,
        minter_token: TOKEN,
        staking_token: TOKEN,
    }
}

/// A carry file from `source`.
fn file(source: CarrySource) -> CarryFile {
    let totals = StateCarried { source, accounts: 0, entitled: TokenUnits(0), staked: TokenUnits(0), awards: 0 };
    let begin = CarryPart::Begin { source, globals: "{}".into(), siblings: Vec::new() };
    CarryFile { source, totals, parts: vec![begin, CarryPart::End] }
}

#[test]
fn every_unmet_condition_of_a_done_move_is_named() {
    assert!(unmet(&done(), &expected()).is_empty());
    let cases: [Unmet; 6] = [
        ("signer", |state| state.source_signer = Address::repeat_byte(1)),
        ("no checkpoint", |state| state.source_checkpoint = None),
        ("registry follows", |state| state.registry_vault = SOURCE),
        ("rotation switch", |state| state.rotation_switch = true),
        ("operator in effect", |state| state.admin_key = Address::repeat_byte(2)),
        ("minter follows", |state| state.minter_vault = SOURCE),
    ];
    for (named, change) in cases {
        let mut state = done();
        change(&mut state);
        let unmet = unmet(&state, &expected());
        assert_eq!(unmet.len(), 1, "{named}: {unmet:?}");
        assert!(unmet[0].contains(named), "{named}: {unmet:?}");
    }
}

#[test]
fn a_registry_the_source_vault_does_not_read_or_a_minter_of_another_token_is_refused() {
    assert!(check_kept(&done(), REGISTRY).is_ok());
    let other = MoveState { minter_token: Address::repeat_byte(0x66), ..done() };
    assert!(format!("{:#}", check_kept(&other, REGISTRY).unwrap_err()).contains("names another minter"));
    let refused = check_kept(&done(), Address::repeat_byte(0x78)).unwrap_err();
    assert!(format!("{refused:#}").contains("the source vault reads registry"), "{refused:#}");
}

#[test]
fn a_file_carries_only_from_the_source_vaults_last_accepted_checkpoint() {
    let accepted = CarrySource { chain_id: 42_161, vault: AccountId(SOURCE.into_array()), seq: 70, root: ROOT };
    assert!(check_source(&file(accepted), (70, ROOT), &expected()).is_ok());
    for other in [
        CarrySource { seq: 69, ..accepted },
        CarrySource { root: B256::repeat_byte(5), ..accepted },
        CarrySource { vault: AccountId(VAULT.into_array()), ..accepted },
        CarrySource { chain_id: 1, ..accepted },
    ] {
        assert!(check_source(&file(other), (70, ROOT), &expected()).is_err(), "{other:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_start_holds_until_the_move_is_done_and_the_file_is_there() {
    let dir = scratch_dir("carry-hold");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("carry.json");
    let source = CarrySource { chain_id: 42_161, vault: AccountId(SOURCE.into_array()), seq: 70, root: ROOT };
    let reads = Cell::new(0);
    // The first read fails, the signer is retired from the third, and the file appears at the fourth.
    let read_chain = || {
        reads.set(reads.get() + 1);
        let n = reads.get();
        if n == 4 {
            let bytes = serde_json::to_vec(&file(source)).unwrap();
            fs::write(&path, bytes).unwrap();
        }
        let state = MoveState { source_signer: if n < 3 { Address::repeat_byte(1) } else { Address::ZERO }, ..done() };
        async move { if n == 1 { Err(anyhow!("no quorum")) } else { Ok(state) } }
    };
    let started = tokio::time::Instant::now();
    let (state, read_back) = hold(read_chain, &expected(), &path).await.unwrap();
    assert_eq!((state, read_back.source), (done(), source));
    assert_eq!(reads.get(), 4);
    assert_eq!(started.elapsed(), 3 * RECHECK);
}

#[tokio::test(start_paused = true)]
async fn a_start_refuses_a_file_that_is_not_a_carry_file() {
    let dir = scratch_dir("carry-hold-garbled");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("carry.json");
    fs::write(&path, b"{}").unwrap();
    let error = hold(|| async { Ok(done()) }, &expected(), &path).await.unwrap_err();
    assert!(format!("{error:#}").contains("is not a carry file"), "{error:#}");
}

#[test]
fn a_log_carries_only_from_the_source_its_config_names() {
    let source = CarrySource { chain_id: 42_161, vault: AccountId(SOURCE.into_array()), seq: 70, root: ROOT };
    let configured = Some((42_161, AccountId(SOURCE.into_array())));
    assert!(check_carried_source(Some(source), configured, false).is_ok());
    assert!(check_carried_source(None, configured, true).is_ok(), "a fresh log has yet to carry");
    assert!(check_carried_source(None, None, false).is_ok());
    for (carried, configured, fresh) in [
        (Some(source), Some((1, AccountId(SOURCE.into_array()))), false),
        (Some(source), Some((42_161, AccountId(VAULT.into_array()))), false),
        (Some(source), None, false),
        (None, configured, false),
    ] {
        assert!(check_carried_source(carried, configured, fresh).is_err(), "{carried:?} {configured:?}");
    }
}

/// A member's copy is held to its config's carry: one carried from a source the config does not name is refused
/// whenever the member checks, and one that carried nothing, under a config naming a carry, only before a turn,
/// when its genesis has closed, not while the member follows a copy that may be before it.
#[test]
fn a_members_copy_is_held_to_the_carry_its_config_names() {
    let path = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v90-carried.jsonl"));
    let seq = crate::carry::build(path, None).unwrap().source.seq;
    let carried = crate::exit::engine_at_checkpoint(path, seq).unwrap();
    let source = carried.carried_from().expect("the reference log carried");
    let config = |carry: Option<AccountId>| {
        let mut cfg = crate::test_support::dev_config();
        cfg.custody.chain_id = source.chain_id;
        cfg.genesis.carry = carry.map(|vault| crate::config::CarrySection { vault, file: "carry.json".into() });
        cfg
    };
    let (named, other, none) = (config(Some(source.vault)), config(Some(AccountId([0xa1; 20]))), config(None));
    let nothing = Engine::new();
    for open in [false, true] {
        assert!(check_copy(&named, &carried, open).is_ok(), "{open}");
        assert!(check_copy(&other, &carried, open).is_err(), "{open}");
        assert!(check_copy(&none, &carried, open).is_err(), "{open}");
        assert!(check_copy(&none, &nothing, open).is_ok(), "{open}");
    }
    assert!(check_copy(&named, &nothing, false).is_err(), "a closed genesis that carried nothing");
    assert!(check_copy(&named, &nothing, true).is_ok(), "a copy that may be before its genesis");
}
