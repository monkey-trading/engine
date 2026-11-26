//! Startup refuses a schedule's governance record that names another vote or fault proof contract than the
//! config, and a record that cannot be read, with a Critical alert for each, before anything is written; a
//! record that names the configured contracts or none, a schedule that keeps none, and a config naming
//! neither contract let it go on.

use alloy::primitives::Address;

use crate::custody::{GovernanceRecord, RecordReader};
use crate::node::preflight::governance_preflight;
use crate::test_support::capture_alerts;

const VOTE: Address = Address::repeat_byte(0x5f);
const FAULT_PROOF: Address = Address::repeat_byte(0x5e);
const LOOKALIKE: Address = Address::repeat_byte(0x6e);

#[tokio::test]
async fn startup_refuses_a_record_naming_another_contract_and_an_unreadable_one() {
    let (_guard, mut alerts) = capture_alerts();
    let configured = (Some(VOTE), Some(FAULT_PROOF));
    let refused = [
        GovernanceRecord::Set { vote: LOOKALIKE, fault_proof: FAULT_PROOF },
        GovernanceRecord::Set { vote: VOTE, fault_proof: LOOKALIKE },
    ];
    for record in refused {
        let error = governance_preflight(Some(&RecordReader::fixed(configured, Some(record)))).await.unwrap_err();
        assert!(format!("{error:#}").contains(&format!("contract {LOOKALIKE}, not the configured")), "{error:#}");
        let alert = alerts.try_recv().expect("a refusal raises Critical");
        assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("governance_record_mismatch", "ERROR"));
    }
    governance_preflight(Some(&RecordReader::fixed(configured, None))).await.unwrap_err();
    let alert = alerts.try_recv().expect("an unread record raises Critical");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("checkpoint_startup_failed", "ERROR"));
}

#[tokio::test]
async fn a_record_naming_the_configured_contracts_or_none_or_kept_by_no_schedule_lets_the_start_go_on() {
    let (_guard, mut alerts) = capture_alerts();
    let allowed = [
        GovernanceRecord::Set { vote: VOTE, fault_proof: FAULT_PROOF },
        GovernanceRecord::Unset,
        GovernanceRecord::Absent,
    ];
    for record in allowed {
        let reader = RecordReader::fixed((Some(VOTE), Some(FAULT_PROOF)), Some(record));
        assert_eq!(governance_preflight(Some(&reader)).await.unwrap(), Some(record));
    }
    // Only the contracts the config names are held to the record.
    let only_vote =
        RecordReader::fixed((Some(VOTE), None), Some(GovernanceRecord::Set { vote: VOTE, fault_proof: LOOKALIKE }));
    governance_preflight(Some(&only_vote)).await.unwrap();
    assert_eq!(governance_preflight(None).await.unwrap(), None, "no reader without a contract to check");
    assert!(alerts.try_recv().is_err(), "an allowed start raised an alert");
}
