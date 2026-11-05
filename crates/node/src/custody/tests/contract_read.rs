//! `ContractQuorum::read`: the calls run at the final block, whose header the time is read from.

use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, B256, Bytes};

use crate::custody::chain::BlockLink;
use crate::custody::evm::{BlockRef, Finality, StakingEvent, VaultDeposit, VaultIdentity, VaultReader};
use crate::custody::quorum::Quorum;
use crate::custody::read_final;
use crate::custody::tests::fakes::FakeChain;

/// A provider over a fake chain that answers the contract call and, once `reorg` is set, serves
/// header reads after its first with another hash for every block, as after a reorg between the
/// finality read and the header read.
#[derive(Clone)]
struct Provider {
    chain: FakeChain,
    reorg: bool,
    header_reads: Arc<Mutex<u32>>,
    calls_at: Arc<Mutex<Vec<u64>>>,
}

impl VaultReader for Provider {
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        self.chain.identity().await
    }

    async fn finality(&self) -> anyhow::Result<Finality> {
        self.chain.finality().await
    }

    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        let read = std::mem::replace(&mut *self.header_reads.lock().unwrap(), 1);
        let mut headers = self.chain.headers(from, to).await?;
        if self.reorg && read > 0 {
            headers.iter_mut().for_each(|link| link.hash = B256::repeat_byte(9));
        }
        Ok(headers)
    }

    async fn deposits(&self, from: u64, to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        self.chain.deposits(from, to).await
    }

    async fn deposit_count(&self, at: BlockRef) -> anyhow::Result<u64> {
        self.chain.deposit_count(at).await
    }

    async fn staking_events(&self, blocks: &[BlockRef]) -> anyhow::Result<Vec<StakingEvent>> {
        self.chain.staking_events(blocks).await
    }

    async fn contract_call(&self, at: BlockRef, _to: Address, _data: Bytes) -> anyhow::Result<Bytes> {
        self.calls_at.lock().unwrap().push(at.number);
        Ok(Bytes::from_static(&[7]))
    }
}

fn quorum(reorg: bool) -> (Quorum<Provider>, Arc<Mutex<Vec<u64>>>) {
    let calls_at = Arc::new(Mutex::new(Vec::new()));
    let providers = (0..3)
        .map(|i| {
            let chain = FakeChain::default();
            {
                let mut state = chain.0.lock().unwrap();
                state.head = 5;
                state.final_block = Some(5);
            }
            let provider = Provider { chain, reorg, header_reads: Arc::default(), calls_at: calls_at.clone() };
            (format!("p{i}"), provider)
        })
        .collect();
    (Quorum::new(providers).unwrap(), calls_at)
}

#[tokio::test]
async fn the_calls_run_at_the_final_block_and_the_time_is_its_headers() {
    let (quorum, calls_at) = quorum(false);
    let (time, answers) =
        read_final(&quorum, Address::repeat_byte(1), |_| vec![("op", Bytes::from_static(&[1]))]).await.unwrap();
    // The fake chain's header for block 5 is stamped 60.
    assert_eq!((time, answers), (60, vec![Bytes::from_static(&[7])]));
    assert_eq!(*calls_at.lock().unwrap(), [5, 5, 5]);
}

#[tokio::test]
async fn a_final_block_whose_agreed_header_hash_differs_is_refused_before_any_call() {
    // The providers agree block 5 is final, but the header they then agree on for it has another hash:
    // its time is not that of the block the calls would run at.
    let (quorum, calls_at) = quorum(true);
    let error =
        read_final(&quorum, Address::repeat_byte(1), |_| vec![("op", Bytes::from_static(&[1]))]).await.unwrap_err();
    assert!(format!("{error:#}").contains("the final block's header"), "{error:#}");
    assert!(calls_at.lock().unwrap().is_empty(), "no call ran");
}
