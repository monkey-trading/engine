use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::sol_types::SolCall;
use engine_core::command::AdminCommand;
use serde_json::json;

use super::*;
use crate::custody::operators::tests::{FINAL_TIME, final_block, honest, json_of, reader};

pub(crate) const VOTE: Address = Address::repeat_byte(0x0d);

/// The answer to a call to the vote contract over `proposals`, noting its selector in `calls`: none,
/// as from an address with no code, while there are no proposals.
/// Error cases:
/// - a call the vote contract does not answer
pub(crate) fn answer(
    proposals: &[ProposalView],
    calls: &Arc<Mutex<Vec<[u8; 4]>>>,
    data: &[u8],
) -> anyhow::Result<Bytes> {
    let selector: [u8; 4] = data[..4].try_into()?;
    calls.lock().unwrap().push(selector);
    if proposals.is_empty() {
        return Ok(Bytes::new());
    }
    if selector == proposalCountCall::SELECTOR {
        return Ok(proposalCountCall::abi_encode_returns(&U256::from(proposals.len())).into());
    }
    let call = proposalsCall::abi_decode(data)?;
    let first = usize::try_from(call.first)?;
    let end = (first - 1 + usize::try_from(call.limit)?).min(proposals.len());
    Ok(proposalsCall::abi_encode_returns(&proposals[first - 1..end].to_vec()).into())
}

/// Proposal `id`, opened `age` seconds before the final block for `window` seconds.
fn proposal(id: u64, age: u64, window: u64, passed: bool) -> ProposalView {
    let command = serde_json::to_vec(&AdminCommand::SetReduceOnly { market: Some(1), reduce_only: true }).unwrap();
    let opens_at = FINAL_TIME - age;
    ProposalView {
        id: U256::from(id),
        proposal: Proposal {
            commandHash: keccak256(&command),
            proposer: Address::repeat_byte(0xa1),
            setId: 1,
            fast: window < 86_400,
            passed,
            opensAt: opens_at,
            closesAt: opens_at + window,
            totalWeight: U256::from(100u64),
            yesWeight: U256::from(40u64),
        },
        command: command.into(),
    }
}

/// Providers whose vote contract holds `proposals`, and a reader of them.
fn reading(proposals: Vec<ProposalView>) -> OperatorsReader<crate::custody::operators::tests::Provider> {
    let mut providers = honest(1);
    for provider in &mut providers {
        provider.model.proposals = proposals.clone();
    }
    let (mut reader, _) = reader(providers, Duration::from_secs(60));
    reader.vote = Some(VOTE);
    reader
}

#[tokio::test]
async fn open_proposals_are_listed_newest_first_and_the_read_stops_past_the_longest_window() {
    let day = 86_400;
    // Ids 1 to 20: 1 opened 40 days ago, the rest each an hour apart, the newest an hour ago.
    let mut proposals = vec![proposal(1, 40 * day, day, false)];
    proposals.extend((2..=20).map(|id| proposal(id, (21 - id) * 3_600, 2 * day, id == 19)));
    // 18 opened three hours ago on the fast path and closed after an hour.
    proposals[17] = proposal(18, 3 * 3_600, 3_600, false);
    let list = json_of(&reading(proposals).read().await.unwrap());
    let open: Vec<u64> =
        list["proposals"]["open"].as_array().unwrap().iter().map(|p| p["id"].as_u64().unwrap()).collect();
    assert_eq!(open, [20, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2], "neither passed, closed nor old");
    assert_eq!(list["proposals"]["truncated"], json!(false));
    let first = &list["proposals"]["open"][0];
    assert_eq!(first["command"], json!({ "action": "set_reduce_only", "market": 1, "reduce_only": true }));
    assert_eq!((first["yes_weight"].clone(), first["total_weight"].clone()), (json!("40"), json!("100")));
    assert_eq!(first["closes_at_ms"], json!((FINAL_TIME - 3_600 + 2 * day) * 1000));
}

#[tokio::test]
async fn the_read_stops_one_page_past_the_first_proposal_older_than_the_longest_window() {
    let day = 86_400;
    // Ids 1 to 16, two pages, opened 40 days ago, below ids 17 to 24 opened an hour ago.
    let mut proposals: Vec<_> = (1..=16).map(|id| proposal(id, 40 * day, day, false)).collect();
    proposals.extend((17..=24).map(|id| proposal(id, 3_600, 2 * day, false)));
    let mut providers = honest(1);
    for provider in &mut providers {
        provider.model.proposals = proposals.clone();
    }
    let (mut reader, _) = reader(providers.clone(), Duration::from_secs(60));
    reader.vote = Some(VOTE);
    let list = json_of(&reader.read().await.unwrap());
    let open: Vec<u64> =
        list["proposals"]["open"].as_array().unwrap().iter().map(|p| p["id"].as_u64().unwrap()).collect();
    assert_eq!(open, (17..=24).rev().collect::<Vec<_>>());
    assert_eq!(list["proposals"]["truncated"], json!(false));
    // The page of 17 to 24, then the page of 9 to 16, whose proposals are past the longest window, and
    // no page of 1 to 8. The three providers share one log: each call is made to every one of them.
    let pages = providers[0].calls().iter().filter(|selector| **selector == proposalsCall::SELECTOR).count();
    assert_eq!(pages, 2 * 3);
}

#[tokio::test]
async fn a_read_stops_at_its_page_limit_and_says_so() {
    let proposals: Vec<_> = (1..=80).map(|id| proposal(id, 3_600, 86_400, false)).collect();
    let list = json_of(&reading(proposals).read().await.unwrap());
    assert_eq!(list["proposals"]["open"].as_array().unwrap().len(), 64);
    assert_eq!(list["proposals"]["truncated"], json!(true));
}

#[tokio::test]
async fn a_failed_proposals_read_serves_the_operator_list_with_them_unavailable() {
    let mut proposals = vec![proposal(1, 3_600, 86_400, false)];
    proposals[0].proposal.commandHash = B256::repeat_byte(1);
    let reader = reading(proposals);
    let list = json_of(&reader.read().await.unwrap());
    let unavailable = json!({ "vote": format!("{VOTE:#x}"), "open": [], "truncated": false, "unavailable": true });
    assert_eq!((list["candidates"].as_array().unwrap().len(), &list["proposals"]), (1, &unavailable));
    // The read the list went without fails on the command that does not hash to its proposal's.
    let error = reader.read_proposals(VOTE, final_block(), FINAL_TIME as i64 * 1000).await.unwrap_err();
    assert!(format!("{error:#}").contains("does not hash to its hash"), "{error:#}");
}

#[tokio::test]
async fn a_vote_contract_with_no_code_holds_no_proposal() {
    let list = json_of(&reading(Vec::new()).read().await.unwrap());
    assert_eq!(list["proposals"], json!({ "vote": format!("{VOTE:#x}"), "open": [], "truncated": false }));
}

#[tokio::test]
async fn bytes_that_are_no_canonical_command_list_with_no_command() {
    let mut proposals = vec![proposal(1, 3_600, 86_400, false)];
    let bytes = Bytes::from_static(br#"{"action": "set_reduce_only", "market": 1, "reduce_only": true}"#);
    proposals[0].proposal.commandHash = keccak256(&bytes);
    proposals[0].command = bytes;
    let list = json_of(&reading(proposals).read().await.unwrap());
    assert_eq!(list["proposals"]["open"][0]["command"], json!(null));
}
