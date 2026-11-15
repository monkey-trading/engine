use super::*;

/// An operator `byte` names: its candidate, release, hostname and handover endpoint, the endpoint
/// empty when `endpoint` is false.
fn operator(byte: u8, endpoint: bool) -> HandoverSource {
    HandoverSource {
        candidate: AccountId([byte; 20]),
        release: [byte; 32],
        api_hostname: format!("{byte}.test"),
        handover_endpoint: if endpoint { format!("{byte}.test:7443") } else { String::new() },
    }
}

fn member(byte: u8, held: bool) -> Registered {
    Registered { source: operator(byte, true), held }
}

fn candidates(sources: &[HandoverSource]) -> Vec<u8> {
    sources.iter().map(|source| source.candidate.0[0]).collect()
}

#[test]
fn the_head_holder_comes_first_then_members_that_held_the_turn_then_earlier_holders() {
    // A, the source, holds the turn before; B was drawn and took it late, then C was skipped to; the
    // newest head is B's.
    let members = [member(0xa, false), member(0xb, true), member(0xc, true), member(0xd, false)];
    let earlier = vec![operator(0xa, true), operator(0xe, true)];
    let by_b = ordered(AccountId([0xc; 20]), Some([0xb; 32]), &members, earlier.clone());
    assert_eq!(candidates(&by_b), [0xb, 0xa, 0xe], "B by its head, no other held member but C, then A and E");
    // With the newest head A's, A comes first.
    let by_a = ordered(AccountId([0xc; 20]), Some([0xa; 32]), &members, earlier);
    assert_eq!(candidates(&by_a), [0xa, 0xb, 0xe]);
}

#[test]
fn a_head_release_no_operator_runs_and_no_head_leave_the_members_and_earlier_holders() {
    let members = [member(0xb, true), member(0xc, false)];
    let earlier = vec![operator(0xa, true)];
    for head in [None, Some([0x99; 32])] {
        assert_eq!(candidates(&ordered(AccountId([0xc; 20]), head, &members, earlier.clone())), [0xb, 0xa]);
    }
}

#[test]
fn the_head_holder_may_be_an_earlier_turns_holder_outside_the_set() {
    let members = [member(0xb, true), member(0xc, true)];
    let earlier = vec![operator(0xa, true), operator(0xf, true)];
    let found = holder(Some([0xf; 32]), &members, &earlier).map(|source| source.candidate);
    assert_eq!(found, Some(AccountId([0xf; 20])));
    assert_eq!(candidates(&ordered(AccountId([0xc; 20]), Some([0xf; 32]), &members, earlier)), [0xf, 0xb, 0xa]);
}

#[test]
fn the_member_itself_and_operators_without_an_endpoint_are_never_asked_and_none_twice() {
    // The newest head is the member's own: a holder that restarted asks the others.
    let members = [member(0xc, true), Registered { source: operator(0xb, false), held: true }, member(0xa, true)];
    let earlier = vec![operator(0xa, true), operator(0xc, true)];
    assert_eq!(candidates(&ordered(AccountId([0xc; 20]), Some([0xc; 32]), &members, earlier)), [0xa]);
}
