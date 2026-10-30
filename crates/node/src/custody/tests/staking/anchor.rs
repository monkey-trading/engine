//! The quorum must bind empty and partial log results to the independently proven endpoint.

use super::*;

/// An admitted endpoint whose log backend can ignore the requested canonical hash.
struct ForkReader {
    endpoint: B256,
    events: Vec<StakingEvent>,
}

impl VaultReader for ForkReader {
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        Ok(VaultIdentity { chain_id: CHAIN_ID, token: None })
    }

    async fn finality(&self) -> anyhow::Result<Finality> {
        Ok(Finality { head: 3, head_time: 0, tagged: None, head_support: None })
    }

    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        Ok((from..=to)
            .map(|number| BlockLink {
                number,
                hash: if number == 3 { self.endpoint } else { canonical_hash(number) },
                parent_hash: B256::ZERO,
                timestamp: number * 12,
            })
            .collect())
    }

    async fn deposits(&self, _from: u64, _to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        Ok(Vec::new())
    }

    async fn deposit_count(&self, _at: BlockRef) -> anyhow::Result<u64> {
        Ok(0)
    }

    async fn staking_events(&self, blocks: &[BlockRef]) -> anyhow::Result<Vec<StakingEvent>> {
        let from = blocks.first().unwrap().number;
        let to = *blocks.last().unwrap();
        // Fault injection: the backend ignores the proven hash and returns its own fork's logs.
        Ok(self.events.iter().filter(|event| (from..=to.number).contains(&event.block)).cloned().collect())
    }
}

#[tokio::test]
async fn fork_bound_empty_and_partial_staking_responses_cannot_advance_canonical_progress() {
    for partial in [false, true] {
        let dir = scratch(if partial { "staking-fork-partial" } else { "staking-fork-empty" });
        let node = Node::start_confirmed(&dir);
        init_staking(&node).await;
        let account = AccountId([0x74; 20]);
        let returned = if partial { vec![staked(2, 0x74, account, 20)] } else { Vec::new() };
        let reader = Quorum::new(
            (0..3)
                .map(|id| {
                    (format!("p{id}"), ForkReader { endpoint: B256::repeat_byte(0xf0), events: returned.clone() })
                })
                .collect(),
        )
        .unwrap();
        reader.identity().await.unwrap();
        let before = node.last_seq().await;
        let mut cursor = cursor_at(1);
        let result = mirror(
            &reader,
            &node.tx,
            &mut cursor,
            &dir.join("staking-cursor.json"),
            &mut AdaptiveSize::new(1_000),
            1..=3,
            &hashes_through(3),
            true,
        )
        .await;
        assert!(result.is_err(), "a fork endpoint cannot certify canonical completeness");
        assert_eq!(node.last_seq().await, before, "partial events and progress are rejected before sequencing");
        assert_eq!(cursor, cursor_at(1));
        assert!(!dir.join("staking-cursor.json").exists());
        assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(0));
        assert_eq!(node.read(|engine| engine.staking().unwrap().completed_through).await, None);
        node.stop().await;
    }
}

#[tokio::test]
async fn canonical_bound_staking_responses_apply_events_and_complete_the_proven_range() {
    let dir = scratch("staking-proven-endpoint");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([0x75; 20]);
    let reader = Quorum::new(
        (0..3)
            .map(|id| {
                (
                    format!("p{id}"),
                    ForkReader {
                        endpoint: canonical_hash(3),
                        events: {
                            let mut events = vec![staked(1, 0x76, account, 5), staked(2, 0x75, account, 20)];
                            if id % 2 == 0 {
                                events.reverse();
                            }
                            events
                        },
                    },
                )
            })
            .collect(),
    )
    .unwrap();
    reader.identity().await.unwrap();
    let mut cursor = cursor_at(1);
    mirror(
        &reader,
        &node.tx,
        &mut cursor,
        &dir.join("staking-cursor.json"),
        &mut AdaptiveSize::new(1_000),
        1..=3,
        &hashes_through(3),
        true,
    )
    .await
    .unwrap();
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(25));
    let completed = node.read(|engine| engine.staking().unwrap().completed_through.unwrap()).await;
    assert_eq!((completed.block, completed.block_hash), (3, canonical_hash(3)));
    assert_eq!(cursor.next_block, 4);
    node.stop().await;
}
