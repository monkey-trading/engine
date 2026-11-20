use std::collections::BTreeMap;

use super::*;
use crate::sequencer::replay::ranges::stitch::resolve;

/// A range's references resolve against the ranges before it: a typed payout's checkpoint by seq and root,
/// and a deferred approval's release once, each refused when no earlier range holds it.
#[test]
fn a_ranges_references_resolve_once_against_the_ranges_before_it() {
    let (account, amount) = (AccountId([4; 20]), Fixed::from_int(7));
    let roots: BTreeMap<u64, B256> = [(3, B256::repeat_byte(1)), (9, B256::repeat_byte(2))].into();
    let approval = (9, account, amount, 1);
    let refs = Refs { start: Some(12), payouts: vec![(3, B256::repeat_byte(1))], deferred: vec![approval] };
    let mut deferred = vec![(3, account, amount, 0), approval];
    resolve(&refs, &roots, &mut deferred).unwrap();
    assert_eq!(deferred, [(3, account, amount, 0)], "the release took its approval out");
    // Released again, or never deferred, or a payout at a seq no range holds or with another root.
    for (case, refs) in [
        ("a second release", Refs { deferred: vec![approval], ..Refs::default() }),
        ("an unknown checkpoint", Refs { payouts: vec![(4, B256::repeat_byte(1))], ..Refs::default() }),
        ("another root", Refs { payouts: vec![(9, B256::repeat_byte(1))], ..Refs::default() }),
    ] {
        assert!(resolve(&refs, &roots, &mut deferred.clone()).is_err(), "{case} resolved");
    }
}

/// The ranges' results come in log order whatever order their threads finish in, and once a range fails no
/// later range is taken: the results end at the earliest failure, which is the fault reported.
#[test]
fn ranges_report_in_log_order_and_end_at_the_earliest_failure() {
    let ranges: Vec<Range> = (0..8).map(|_| Range { start: None, end: None }).collect();
    let at = |range: &Range| ranges.iter().position(|held| std::ptr::eq(held, range)).unwrap();
    for jobs in [1, 3, 8] {
        let jobs = NonZeroUsize::new(jobs).unwrap();
        let all = run(&ranges, jobs, |range, last| anyhow::Ok((at(range), last)));
        let all: Vec<_> = all.into_iter().map(Result::unwrap).collect();
        assert_eq!(all, (0..8).map(|n| (n, n == 7)).collect::<Vec<_>>(), "{jobs} jobs");
        let failing = run(&ranges, jobs, |range, _| {
            let n = at(range);
            if n == 2 || n == 5 { anyhow::bail!("range {n}") } else { anyhow::Ok(n) }
        });
        assert_eq!(failing.len(), 3, "{jobs} jobs: the results end at the earliest failure");
        assert_eq!(failing[2].as_ref().unwrap_err().to_string(), "range 2", "{jobs} jobs");
    }
}

/// The pass takes the ranges as one history only where each starts from the checkpoint and the state the
/// one before ends in: a range from no snapshot, at another checkpoint, or from another state is refused.
#[test]
fn each_range_starts_from_the_checkpoint_and_state_the_one_before_ends_in() {
    let at = |seq, state| {
        let checkpoint = Checkpoint {
            parent: None,
            seq,
            root: format!("0x{}", hex::encode([7; 32])),
            signature: "0x01".into(),
            signer: "0x02".into(),
            domain: None,
            deferred_withdrawals: false,
        };
        Boundary { checkpoint, state: [state; 32] }
    };
    let range = |start, end| {
        let mut replayed = Replay::default().finish();
        replayed.checkpoints = 1;
        Ok(RangeReplayed { replayed, refs: None, anchored: false, start, end })
    };
    let trust = Trust::Pinned { domain: None, signer: None };
    stitch::stitch(vec![range(None, Some(at(4, 1))), range(Some(at(4, 1)), None)], 2, &trust).unwrap();
    for (case, start) in
        [("no snapshot", None), ("another checkpoint", Some(at(5, 1))), ("another state", Some(at(4, 2)))]
    {
        let error = stitch::stitch(vec![range(None, Some(at(4, 1))), range(start, None)], 2, &trust)
            .err()
            .unwrap_or_else(|| panic!("{case} was stitched"));
        assert!(error.to_string().contains("range 2 of 2 starts from another checkpoint or state"), "{case}: {error}");
    }
}
