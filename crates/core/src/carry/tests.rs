use alloy_primitives::B256;

use super::*;
use crate::command::AdminCommand;

#[test]
fn every_part_keeps_its_encoding_through_an_admin_command() {
    let source = CarrySource { chain_id: 42_161, vault: AccountId([0xa1; 20]), seq: 7, root: B256::repeat_byte(3) };
    let balance = CarriedBalance { account: AccountId([1; 20]), amount: TokenUnits(5), siblings: vec![B256::ZERO] };
    let absence = AbsenceProof { other: Some((B256::repeat_byte(4), B256::repeat_byte(5))), siblings: vec![] };
    for part in [
        CarryPart::Begin { source, globals: "{\"last_seq\":7}".into(), siblings: vec![B256::repeat_byte(9)] },
        CarryPart::Entitlements { entries: vec![balance.clone()] },
        CarryPart::Stakes { entries: vec![balance] },
        CarryPart::Awards { awards: vec![], next: Some(absence) },
        CarryPart::Awards { awards: vec![], next: None },
        CarryPart::End,
    ] {
        let command = AdminCommand::CarryState(part);
        let json = serde_json::to_string(&command).expect("a carry part serialises");
        let back: AdminCommand = serde_json::from_str(&json).expect("a carry part parses");
        assert_eq!(back, command, "{json}");
        assert_eq!(serde_json::to_string(&back).expect("serialises"), json);
    }
}

#[test]
fn a_part_naming_a_field_it_does_not_hold_is_refused() {
    for json in [
        r#"{"action":"carry_state","part":"stakes","entries":[],"next":null}"#,
        r#"{"action":"carry_state","part":"awards","awards":[],"entries":[]}"#,
        r#"{"action":"carry_state","part":"entitlements","entries":[{"account":"0x0101010101010101010101010101010101010101","amount":"1","siblings":[],"extra":1}]}"#,
    ] {
        assert!(serde_json::from_str::<AdminCommand>(json).is_err(), "{json}");
    }
}
