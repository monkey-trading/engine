//! The scan's refusal to finalise at an end the release tag's chain does not hold: the headers above the
//! end that do not link it to the tag, and a stored window end at the tag's height with another hash.

use super::*;

/// The first block range each chain's deposit reads asked for since `from`.
fn first_read_since(chain: &FakeChain, from: usize) -> (u64, u64) {
    chain.reads()[from]
}

/// The finalised frontier's block.
async fn frontier(node: &Node) -> Option<u64> {
    node.read(|e| e.custody_checkpoint().map(|point| point.block)).await
}

/// A scan with windows of four blocks that waits two confirmations behind its head, whose frontier is
/// finalised at block 2 and whose head and release tag then move to block 18, the scan reading blocks 3
/// to 14 and then 15 and 16, the eligible head.
async fn behind_the_tag(dir: &Path, node: &Node) -> ([FakeChain; 3], DepositWatcher<Quorum<FakeChain>>) {
    let chains = chains(4, Some(4));
    let mut cfg: EvmCustodySection = toml::from_str(&evm_toml(dir, 4)).unwrap();
    cfg.confirmations = 2;
    let quorum =
        Quorum::new(chains.iter().enumerate().map(|(i, chain)| (format!("p{i}"), chain.clone())).collect()).unwrap();
    let mut scan = DepositWatcher::start_unscreened(quorum, &cfg, CHAIN_ID, VAULT, None).unwrap();
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(frontier(node).await, Some(2), "the frontier is finalised at the eligible head");
    set_head(&chains, 18);
    set_release(&chains, Some(18));
    (chains, scan)
}

#[tokio::test]
async fn a_scan_end_the_headers_do_not_link_to_the_release_tag_is_not_finalised() {
    let dir = scratch("release-unlinked-end");
    let node = Node::start_confirmed(&dir);
    let (chains, mut scan) = behind_the_tag(&dir, &node).await;
    // Block 17's header names a parent off the chain, so the headers above the scan's end, once it is
    // within a block range of the release tag, do not lead from it to the tag's block.
    for chain in &chains {
        chain.0.lock().unwrap().broken_link = Some(17);
    }
    for _ in 0..20 {
        scan.poll(&node.tx).await.unwrap();
        if chains[0].0.lock().unwrap().header_reads.contains(&(15, 18)) {
            break;
        }
    }
    assert!(chains[0].0.lock().unwrap().header_reads.contains(&(15, 18)), "the link to the release tag was read");
    assert_eq!(frontier(&node).await, Some(2), "the frontier does not move");
    let before = chains[0].reads().len();
    scan.poll(&node.tx).await.unwrap();
    assert_eq!(first_read_since(&chains[0], before), (3, 6), "the scan starts again above the finalised frontier");
    assert_eq!(frontier(&node).await, Some(2), "nothing is finalised");
    for chain in &chains {
        chain.0.lock().unwrap().broken_link = None;
    }
    catch_up(&mut scan, &node.tx).await;
    scan.poll(&node.tx).await.unwrap();
    assert_eq!(frontier(&node).await, Some(16), "once the headers link, the frontier moves to the scan's end");
    node.stop().await;
}

#[tokio::test]
async fn a_stored_window_end_at_the_release_height_with_another_hash_is_not_finalised() {
    let dir = scratch("release-end-differs");
    let node = Node::start_confirmed(&dir);
    let chains = chains(2, Some(2));
    let mut scan = watcher(&chains, &dir, 4, None);
    scan.poll(&node.tx).await.unwrap();
    set_head(&chains, 14);
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(frontier(&node).await, Some(2));
    // The release tag names block 10, the middle stored end, by a hash the chain now serves; the scan's top
    // end, which a poll's boundary read checks, still matches, so only the release check sees it.
    for chain in &chains {
        fork(chain, 10, 10);
    }
    set_release(&chains, Some(10));
    let before = chains[0].reads().len();
    scan.poll(&node.tx).await.unwrap();
    assert_eq!(frontier(&node).await, Some(2), "the frontier does not move");
    scan.poll(&node.tx).await.unwrap();
    assert_eq!(first_read_since(&chains[0], before), (3, 6), "the scan starts again above the finalised frontier");
    node.stop().await;
}
