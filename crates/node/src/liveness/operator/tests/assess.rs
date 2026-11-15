//! How the operator weighs the members' statuses against its log.

use std::collections::BTreeSet;

use super::*;

#[test]
fn the_assessment_confirms_matching_copies_and_names_longer_or_conflicting_ones() {
    let prefixes = BTreeMap::from([(8, link(8)), (10, link(10))]);
    let prefix_at = |seq| prefixes.get(&seq).copied();
    let tip = Head { seq: 10, link: link(10) };
    let none = BTreeSet::new();
    let status = |durable: Head, seen: Option<Head>| Status { epoch: 1, durable: Some(durable), sealer: OWN, seen };
    let at = |seq| Head { seq, link: link(seq) };
    let assessment =
        assess(&[(account(1), status(at(8), None)), (account(2), status(at(10), None))], prefix_at, tip, (OWN, &none));
    assert_eq!(assessment.answered, 2);
    assert_eq!(assessment.confirmed, Some((10, account(2))));
    assert_eq!((assessment.beyond, assessment.conflict), (None, None));
    let assessment = assess(&[(account(1), status(at(8), Some(at(12))))], prefix_at, tip, (OWN, &none));
    assert_eq!(assessment.beyond, Some((12, account(1))), "a head it has seen past this log's tip");
    assert_eq!(assessment.confirmed, Some((8, account(1))), "its durable copy still confirms");
    let forked = Head { seq: 8, link: B256::repeat_byte(0xee) };
    let assessment = assess(&[(account(1), status(forked, None))], prefix_at, tip, (OWN, &none));
    assert_eq!((assessment.conflict, assessment.confirmed), (Some((8, account(1))), None));
    let seen_fork = status(at(10), Some(forked));
    let assessment =
        assess(&[(account(1), seen_fork), (account(2), status(at(8), None))], prefix_at, tip, (OWN, &none));
    assert_eq!(assessment.conflict, Some((8, account(1))));
    assert_eq!(assessment.confirmed, Some((8, account(2))), "a member that saw a conflicting head confirms nothing");
    // A failed operator's tail its own earlier key sealed, held by a member that held the turn, at
    // another link or past the tip, is no copy of this key; the same at this key's seal is a second
    // history.
    let foreign = |durable: Head| Status { epoch: 1, durable: Some(durable), sealer: account(7), seen: None };
    let held = BTreeSet::from([account(1), account(2)]);
    let both = [(account(1), foreign(forked)), (account(2), foreign(at(12)))];
    let assessment = assess(&both, prefix_at, tip, (OWN, &held));
    assert_eq!((assessment.conflict, assessment.beyond, assessment.confirmed), (None, None, None));
    // The same tail held by a member that never held the turn is what it confirmed to the outgoing
    // operator: it holds seqs this log lacks, or another history.
    let assessment = assess(&both, prefix_at, tip, (OWN, &none));
    assert_eq!((assessment.conflict, assessment.beyond), (Some((8, account(1))), Some((12, account(2)))));
    let assessment = assess(&[(account(1), foreign(at(10)))], prefix_at, tip, (OWN, &held));
    assert_eq!(assessment.confirmed, Some((10, account(1))), "a matching copy confirms whoever sealed it");
    let own_fork = [(account(1), Status { sealer: OWN, ..foreign(forked) })];
    let assessment = assess(&own_fork, prefix_at, tip, (OWN, &held));
    assert_eq!(assessment.conflict, Some((8, account(1))), "this key's own fork conflicts");
    let unknown = Head { seq: 9, link: link(9) };
    let assessment = assess(&[(account(1), status(unknown, None))], prefix_at, tip, (OWN, &none));
    assert_eq!((assessment.confirmed, assessment.conflict), (None, None), "no prefix retained there: neither");
}
