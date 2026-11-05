//! Log-head reads through the custody quorum: block and head at the highest height a strict
//! majority agrees on, never the custody finality tag, falling back past a faulty provider, a
//! lagging minority excluded, and one quorum loss when no height wins; startup's walk back through
//! the epochs the starting key signed; the log-head and relayer balance quorums alert under their
//! own names at Warning.

use alloy::primitives::{Address, B256};

use crate::custody::tests::fakes::*;
use crate::custody::tests::quorum::{chains, quorum};

#[tokio::test]
async fn the_log_head_read_uses_a_majority_latest_block_not_the_finality_tag() {
    let chains = chains();
    // A post lands at block five; the chain's head is ten and the finality tag still names block
    // one, as a fresh chain's does. The read must see the post at the latest block — a finality-
    // tagged read would ask block one, before the post, and answer no head.
    for chain in &chains {
        chain.0.lock().unwrap().head = 10;
        chain.0.lock().unwrap().epoch = 1;
        chain.0.lock().unwrap().posted = Some((7, B256::from([7; 32])));
        chain.0.lock().unwrap().posted_block = Some(5);
    }
    let quorum = quorum(&chains).await;
    let (epoch, head) = crate::log_heads::posted(&quorum, Address::repeat_byte(0x11), CHAIN_ID).await.unwrap();
    assert_eq!(epoch, 1);
    assert_eq!(head, Some(crate::log_heads::Head { seq: 7, link: B256::from([7; 32]) }));
    for chain in &chains {
        assert_eq!(
            chain.0.lock().unwrap().head_reads,
            vec![(10, canonical_hash(10))],
            "the head is read at the majority-agreed latest block, never the finality tag"
        );
    }
}

#[tokio::test]
async fn a_lagging_latest_block_minority_cannot_vote_on_the_log_head_read() {
    let chains = chains();
    for chain in &chains {
        chain.0.lock().unwrap().head = 10;
        chain.0.lock().unwrap().epoch = 1;
    }
    // The majority holds the newer head, visible from block three; the minority still sees block
    // nine and the older head it held there. Both votes take the majority's answer, read at block
    // ten: a finality-tagged read would ask block one and see neither post.
    for chain in &chains[..2] {
        chain.0.lock().unwrap().posted = Some((7, B256::from([7; 32])));
        chain.0.lock().unwrap().posted_block = Some(3);
    }
    chains[2].0.lock().unwrap().latest = Some((9, canonical_hash(9)));
    chains[2].0.lock().unwrap().posted = Some((5, B256::from([5; 32])));
    chains[2].0.lock().unwrap().posted_block = Some(2);
    let quorum = quorum(&chains).await;
    let (_, head) = crate::log_heads::posted(&quorum, Address::repeat_byte(0x11), CHAIN_ID).await.unwrap();
    assert_eq!(head, Some(crate::log_heads::Head { seq: 7, link: B256::from([7; 32]) }));
    for chain in &chains[..2] {
        assert_eq!(chain.0.lock().unwrap().head_reads, vec![(10, canonical_hash(10))]);
    }
}

#[tokio::test]
async fn providers_at_different_tips_agree_at_the_majoritys_common_height() {
    // Polled together on a fast chain, three providers answer three tips: the read votes the hash
    // at the second-highest, which a strict majority has reached, and reads the head there.
    let chains = chains();
    for (chain, head) in chains.iter().zip([10, 11, 12]) {
        let mut state = chain.0.lock().unwrap();
        state.head = head;
        state.epoch = 1;
        state.posted = Some((7, B256::from([7; 32])));
        state.posted_block = Some(3);
    }
    let quorum = quorum(&chains).await;
    let (_, head) = crate::log_heads::posted(&quorum, Address::repeat_byte(0x11), CHAIN_ID).await.unwrap();
    assert_eq!(head, Some(crate::log_heads::Head { seq: 7, link: B256::from([7; 32]) }));
    for chain in &chains {
        assert_eq!(chain.0.lock().unwrap().head_reads, vec![(11, canonical_hash(11))], "read at the common height");
    }
}

#[tokio::test]
async fn a_faulty_high_tip_cannot_deny_the_read_when_honest_providers_agree_lower() {
    // Honest tips 10 and 11; a faulty provider claims 999 and fails its header read at 11, the
    // highest height a majority has reached. No majority agrees there, so the read falls to 10,
    // where both honest providers agree.
    let chains = chains();
    for (chain, head) in chains.iter().zip([10, 11, 999]) {
        let mut state = chain.0.lock().unwrap();
        state.head = head;
        state.epoch = 1;
        state.posted = Some((7, B256::from([7; 32])));
        state.posted_block = Some(3);
    }
    let quorum = quorum(&chains).await;
    chains[2].0.lock().unwrap().header_failures = 1;
    let (_, head) = crate::log_heads::posted(&quorum, Address::repeat_byte(0x11), CHAIN_ID).await.unwrap();
    assert_eq!(head, Some(crate::log_heads::Head { seq: 7, link: B256::from([7; 32]) }));
    for chain in &chains[..2] {
        assert_eq!(chain.0.lock().unwrap().head_reads, vec![(10, canonical_hash(10))], "read at the agreed height");
    }
}

#[tokio::test]
async fn a_faulty_provider_refusing_the_head_cannot_deny_the_read_when_honest_providers_agree_lower() {
    // Honest tips 10 and 11; a faulty provider claims 999, agrees on the block hash at 11, then
    // refuses the head read. The provider at 10 cannot serve block 11, so no majority reads the head
    // there; the read falls to 10, where both honest providers read it.
    let chains = chains();
    for (chain, head) in chains.iter().zip([10, 11, 999]) {
        let mut state = chain.0.lock().unwrap();
        state.head = head;
        state.epoch = 1;
        state.posted = Some((7, B256::from([7; 32])));
        state.posted_block = Some(3);
    }
    let quorum = quorum(&chains).await;
    chains[2].0.lock().unwrap().refuse_head_reads = true;
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let (_, head) = crate::log_heads::posted(&quorum, Address::repeat_byte(0x11), CHAIN_ID).await.unwrap();
    assert_eq!(head, Some(crate::log_heads::Head { seq: 7, link: B256::from([7; 32]) }));
    for chain in &chains[..2] {
        assert_eq!(chain.0.lock().unwrap().head_reads.last(), Some(&(10, canonical_hash(10))), "read at 10");
    }
    // The losing candidate voted silently: no quorum loss pages for a read that succeeded.
    let lost: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok())
        .filter(|alert| alert.get("alert").map(String::as_str) == Some("custody_rpc_quorum_lost"))
        .collect();
    assert!(lost.is_empty(), "a candidate that loses raises nothing: {lost:?}");
}

#[tokio::test]
async fn no_candidate_height_with_a_majority_fails_the_read_with_one_quorum_loss() {
    // Every provider is at a different tip and each refuses the header reads below its tip: no
    // height has a strict majority on the block, so the read fails and pages quorum loss once.
    let chains = chains();
    for (chain, head) in chains.iter().zip([10, 11, 12]) {
        let mut state = chain.0.lock().unwrap();
        state.head = head;
        state.epoch = 1;
    }
    let quorum = quorum(&chains).await;
    for chain in &chains {
        chain.0.lock().unwrap().header_failures = u32::MAX;
    }
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    assert!(crate::log_heads::posted(&quorum, Address::repeat_byte(0x11), CHAIN_ID).await.is_err());
    let lost: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok())
        .filter(|alert| alert.get("alert").map(String::as_str) == Some("custody_rpc_quorum_lost"))
        .collect();
    assert_eq!(lost.len(), 1, "one quorum loss for the read: {lost:?}");
    assert_eq!(lost[0].get("operation").map(String::as_str), Some("latest_block"));
}

#[tokio::test]
async fn startup_walks_back_to_the_newest_head_an_unbroken_run_of_this_keys_epochs_holds() {
    // `setSigner` reinstalled this key twice: epoch three is empty, epoch two holds nothing, and
    // epoch one holds the head this key signed over this log. The startup read reaches it, every
    // call at the agreed block.
    let chains = reinstalled([(1, KEY, Some((2, B256::from([2; 32])))), (2, KEY, None), (3, KEY, None)]);
    let quorum = quorum(&chains).await;
    let heads = crate::log_heads::startup_heads(&quorum, LOG_HEADS, VAULT, KEY, CHAIN_ID).await.unwrap();
    assert_eq!((heads.epoch, heads.current), (3, None));
    assert_eq!(heads.newest, Some(crate::log_heads::Head { seq: 2, link: B256::from([2; 32]) }));
    for chain in &chains {
        let reads = chain.0.lock().unwrap().epochs.reads.clone();
        assert!(!reads.is_empty() && reads.iter().all(|(block, _)| *block == 10), "{reads:?}");
    }
}

#[tokio::test]
async fn startup_stops_the_walk_at_an_epoch_another_key_signed() {
    // Epoch two's head is another key's: the run of this key's epochs ends there, so neither it
    // nor epoch one's head is this log's to check.
    let chains = reinstalled([
        (1, KEY, Some((2, B256::from([2; 32])))),
        (2, OTHER, Some((5, B256::from([5; 32])))),
        (3, KEY, None),
    ]);
    let quorum = quorum(&chains).await;
    let heads = crate::log_heads::startup_heads(&quorum, LOG_HEADS, VAULT, KEY, CHAIN_ID).await.unwrap();
    assert_eq!(heads.newest, None);
}

#[tokio::test]
async fn startup_walks_back_only_when_this_key_signs_the_current_epoch() {
    let chains = reinstalled([(2, KEY, Some((5, B256::from([5; 32])))), (3, OTHER, None)]);
    let quorum = quorum(&chains).await;
    let heads = crate::log_heads::startup_heads(&quorum, LOG_HEADS, VAULT, KEY, CHAIN_ID).await.unwrap();
    assert_eq!(heads.newest, None, "another key's epoch starts no walk");
}

#[tokio::test]
async fn startup_reads_no_earlier_epoch_while_the_current_one_holds_a_head() {
    let chains = reinstalled([(2, KEY, Some((5, B256::from([5; 32])))), (3, KEY, None)]);
    for chain in &chains {
        chain.0.lock().unwrap().posted = Some((7, B256::from([7; 32])));
    }
    let quorum = quorum(&chains).await;
    let heads = crate::log_heads::startup_heads(&quorum, LOG_HEADS, VAULT, KEY, CHAIN_ID).await.unwrap();
    let current = Some(crate::log_heads::Head { seq: 7, link: B256::from([7; 32]) });
    assert_eq!((heads.current, heads.newest), (current, current));
    assert!(chains.iter().all(|chain| chain.0.lock().unwrap().epochs.reads.is_empty()));
}

/// The enclave key startup reads for, another key, and the LogHeads contract.
const KEY: Address = Address::repeat_byte(0x5e);
const OTHER: Address = Address::repeat_byte(0x0f);
const LOG_HEADS: Address = Address::repeat_byte(0x11);

/// One epoch's number, its signer, and the head it holds.
type Epoch = (u64, Address, Option<(u64, B256)>);

/// Three providers at block ten in signer epoch three, its own head unposted, with each epoch's
/// signer and head as `history` gives it.
fn reinstalled<const N: usize>(history: [Epoch; N]) -> [FakeChain; 3] {
    let chains = chains();
    for chain in &chains {
        let mut chain = chain.0.lock().unwrap();
        (chain.head, chain.epoch) = (10, 3);
        chain.epochs.by_epoch = history.iter().map(|&(epoch, signer, head)| (epoch, (signer, head))).collect();
    }
    chains
}

#[tokio::test]
async fn the_log_head_and_relayer_balance_quorums_alert_under_their_own_names_at_warning() {
    // Neither read feeds a credit: losing it pages Warning under its own quorum name, so neither
    // shares an episode with the deposit watcher's Critical "crediting paused" quorum loss.
    let cfg = refusing_custody();
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let heads = crate::custody::heads_reader(&cfg.custody).unwrap();
    crate::log_heads::posted(&heads, Address::ZERO, cfg.custody.chain_id).await.unwrap_err();
    crate::custody::BalanceQuorum::connect(&cfg.custody).unwrap().balance(Address::ZERO).await.unwrap_err();
    assert_eq!((logs.warnings("custody_rpc_quorum_lost"), logs.critical("custody_rpc_quorum_lost")), (2, 0));
    let (log_heads, relayer_balance) = (logs.count("quorum=\"log_heads\""), logs.count("quorum=\"relayer_balance\""));
    assert!(log_heads > 0 && relayer_balance > 0, "each quorum raised its own alerts");
    assert_eq!(log_heads + relayer_balance, logs.count("alert=\"custody_rpc_"), "every custody alert names its quorum");
}

/// `config/anvil.toml` with its custody providers on loopback ports nothing listens on, so every
/// read is refused at once.
fn refusing_custody() -> crate::config::NodeConfig {
    let mut anvil = std::fs::read_to_string(crate::test_support::ANVIL_CONFIG).unwrap();
    for port in ["8545", "8546", "8547"] {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        anvil = anvil.replace(&format!("http://127.0.0.1:{port}\""), &format!("http://127.0.0.1:{closed}\""));
    }
    let cfg: crate::config::NodeConfig = toml::from_str(&anvil).unwrap();
    let providers = &cfg.custody.evm.as_ref().unwrap().providers;
    let ports: Vec<_> = providers.iter().map(|p| p.rpc_url.expose().unwrap().port()).collect();
    assert!(ports.iter().all(|port| !matches!(port, Some(8545..=8547))), "no read reaches Anvil: {ports:?}");
    cfg
}
