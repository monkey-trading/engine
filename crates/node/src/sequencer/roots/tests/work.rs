//! A checkpoint's root work on the worker follows what changed since the last root, not how many
//! leaves the tree holds: the commit the sequencer waits on hashes the changed leaves' paths alone.

use super::*;

/// Accounts a credit changes beside the tree's own, at each size.
const CHANGED: u32 = 100;

#[tokio::test]
async fn a_commits_root_work_on_the_worker_follows_what_changed_not_the_trees_size() {
    let mut works = Vec::new();
    for held in [1_000, 20_000] {
        let mut node = Node::new(&format!("roots-work-{held}"), None);
        node.touch(held);
        node.seq.checkpoint().expect("the tree holds every account");
        // The change the measured commit covers: a credit to each of `CHANGED` accounts, each with a
        // reference of its own.
        for index in 0..CHANGED {
            let cmd = Command::Deposit {
                account: account(index),
                amount: Fixed::from_int(1),
                chain_ref: format!("roots-work-{index}"),
            };
            node.seq.sequence(cmd).expect("the deposit applies").expect("trusted time admits it");
        }
        node.seq.flush().expect("the log flushes");
        node.pace_next();
        let (reply, _replied) = oneshot::channel();
        let mut running = node.run(vec![Request::Submit { cmd: Command::Tick, reply: Some(reply) }]);
        let committed = running.logged("the worker's commit", |logged| says(logged, "state root committed")).await;
        works.push((number(&committed, "leaves"), number(&committed, "nodes")));
        let Stopped { ended, .. } = running.stop().await;
        assert!(ended.is_ok(), "the sequencer drains: {ended:?}");
    }
    let [(small_leaves, small_nodes), (large_leaves, large_nodes)] = works[..] else { unreachable!("two sizes") };
    // Each credit changes its account's leaf and adds its reference's, beside the remaining state's.
    let changed = 2 * u64::from(CHANGED) + 1;
    for leaves in [small_leaves, large_leaves] {
        assert!((changed..changed + 40).contains(&leaves), "the commit wrote what changed: {works:?}");
    }
    // Twenty times the leaves take under three times the node hashes: they grow with the depth.
    assert!(small_nodes < large_nodes && large_nodes < 3 * small_nodes, "{works:?}");
}
