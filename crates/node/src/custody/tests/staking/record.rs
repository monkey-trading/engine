//! The start's read of the schedule's governance record: at the release tag's final block, through the custody
//! quorum of providers that serve the configured chain and token, and only while the registry, deployed there,
//! names the configured schedule.

use alloy::primitives::Address;

use super::*;
use crate::custody::evm::GovernanceRecord;
use crate::custody::record::read;
use crate::custody::tests::fakes::USDC_TOKEN;

const SCHEDULE: Address = Address::repeat_byte(0x5c);

#[tokio::test]
async fn the_start_reads_the_record_at_the_release_tag_while_the_registry_names_the_configured_schedule() {
    let set = GovernanceRecord::Set { vote: Address::repeat_byte(0x5f), fault_proof: Address::repeat_byte(0x5e) };
    let chains = poll_chains(12);
    let quorum =
        || Quorum::new(chains.iter().enumerate().map(|(i, chain)| (format!("p{i}"), chain.clone())).collect()).unwrap();
    let set_each = |change: &dyn Fn(&mut ChainState)| {
        for chain in &chains {
            change(&mut chain.0.lock().unwrap());
        }
    };
    let configured = (CHAIN_ID, USDC_TOKEN);
    // The registry names the schedule, whose record names both contracts from block 11, at or below the
    // release tag, which trails the credit tag.
    set_each(&|state| {
        state.schedule.named = SCHEDULE;
        state.schedule.record = Some((11, set));
        state.withdrawal_block = Some(11);
    });
    assert_eq!(read(&quorum(), SCHEDULE, configured).await.unwrap(), set);
    // Below the record's block at the release tag, it names nothing yet, whatever the credit tag holds.
    set_each(&|state| state.withdrawal_block = Some(10));
    assert_eq!(read(&quorum(), SCHEDULE, configured).await.unwrap(), GovernanceRecord::Unset);
    // While the registry names another schedule, or none, nothing vouches for the configured one's record.
    set_each(&|state| state.withdrawal_block = Some(11));
    for named in [Address::repeat_byte(0x6c), Address::ZERO] {
        set_each(&|state| state.schedule.named = named);
        assert_eq!(read(&quorum(), SCHEDULE, configured).await.unwrap(), GovernanceRecord::Unset, "{named}");
    }
    // Nor before the registry's deployment, where it has no code.
    set_each(&|state| {
        state.schedule.named = SCHEDULE;
        state.staking_deployed_at = Some(12);
    });
    assert_eq!(read(&quorum(), SCHEDULE, configured).await.unwrap(), GovernanceRecord::Unset);
    set_each(&|state| state.staking_deployed_at = None);
    // Providers that serve another chain answer nothing.
    set_each(&|state| state.schedule.named = SCHEDULE);
    let error = read(&quorum(), SCHEDULE, (1, USDC_TOKEN)).await.unwrap_err();
    assert!(format!("{error:#}").contains("not the configured chain 1"), "{error:#}");
}
