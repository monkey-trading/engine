//! The admin route's checks against what the contracts hold: an emission schedule's period budget
//! against the vault's MONKEY cap, and a staking config's epoch and cooldown against the staking
//! contract's. A refused command stays out of the log.

use super::*;
use engine_core::Fixed;
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::staking::StakingConfig;

use crate::custody::StakingTerms;

#[tokio::test]
async fn an_emission_schedule_must_have_the_period_budget_the_vaults_cap_was_derived_from() {
    let mut node = Node::start("emission-budget").await;
    node.state.emission_budget = Some(TokenUnits(1_000));
    let init =
        |budget| AdminCommand::InitEmissions(EmissionSchedule { start_ms: NOW, period_budget: TokenUnits(budget) });
    let (status, body) =
        node.post("/v1/admin", serde_json::to_value(node.sign(0, init(1_001), NOW + 60_000)).unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("1001 differs from the vault's 1000"), "{body}");
    assert!(admin_entries(&node.entries()).is_empty(), "a refused schedule stays out of the log");

    let (status, events) =
        node.post("/v1/admin", serde_json::to_value(node.sign(0, init(1_000), NOW + 60_000)).unwrap()).await;
    assert_eq!((status, events), (StatusCode::OK, serde_json::json!([{ "type": "admin_applied" }])));
}

/// The contract's epoch and cooldown, in seconds, as the decided launch values.
const CONTRACT: (u64, u64) = (86_400, 2_419_200);

fn init_staking(epoch_ms: Millis, cooldown_ms: Millis) -> AdminCommand {
    let liquidity_floor = Fixed::from_int(30_500_000);
    AdminCommand::InitStaking(StakingConfig { liquidity_floor, epoch_ms, start_ms: NOW, cooldown_ms })
}

#[tokio::test]
async fn a_staking_config_must_have_the_epoch_and_cooldown_the_staking_contract_holds() {
    let mut node = Node::start_on("staking-terms", true).await;
    node.state.staking_terms = Some(StakingTerms::fixed(Some(CONTRACT)));
    for (command, wrong) in [
        (init_staking(1_800_000, 2_419_200_000), "epoch 1800000 ms differs from the staking contract's 86400 s"),
        (init_staking(86_400_000, 86_400_000), "cooldown 86400000 ms differs from the staking contract's 2419200 s"),
    ] {
        let (status, body) = send(&node, 0, command).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.to_string().contains(wrong), "{body}");
        assert!(admin_entries(&node.entries()).is_empty(), "a refused config stays out of the log");
    }

    let (status, events) = send(&node, 0, init_staking(86_400_000, 2_419_200_000)).await;
    assert_eq!((status, events), (StatusCode::OK, serde_json::json!([{ "type": "admin_applied" }])));
}

#[tokio::test]
async fn a_staking_config_waits_while_the_staking_contract_cannot_be_read() {
    let mut node = Node::start_on("staking-terms-unread", true).await;
    node.state.staking_terms = Some(StakingTerms::fixed(None));
    let body = serde_json::to_value(node.sign(0, init_staking(86_400_000, 2_419_200_000), NOW + 60_000)).unwrap();
    let (status, body) = node.post("/v1/admin", body).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(admin_entries(&node.entries()).is_empty(), "nothing is sequenced on a contract nobody could read");
}

/// `command` posted to the admin route, signed with the operator's `nonce`.
pub(super) async fn send(node: &Node, nonce: u64, command: AdminCommand) -> (StatusCode, Value) {
    node.post("/v1/admin", serde_json::to_value(node.sign(nonce, command, NOW + 60_000)).unwrap()).await
}
