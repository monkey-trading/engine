//! Who earns the operator's share as elections, handovers, cancellations and signed rotations move the
//! operator in effect.

use super::*;

#[test]
fn a_waiting_election_leaves_the_share_with_the_operator_in_effect() {
    let mut h = rotating(1_000_000_000);
    let effective_at = h.ts + NOTICE_MS + 5 * DAY_MS;
    elect(&mut h, 1, CANDIDATE, key(), effective_at);
    stake(&mut h, STAKER_A, 1);
    // Logged and pending: the deployer key still earns, not the waiting candidate.
    let events = close(&mut h);
    assert_eq!(share(&events), Some((operator().address, UsdcUnits(200_000_000))));
    assert_eq!(leaf(&h, CANDIDATE), 0);
}

#[test]
fn a_handover_due_at_the_close_pays_the_new_operator_in_the_same_command() {
    // The election's time is the epoch's end, or just after it and before the close runs: the close
    // pays whoever is in effect when it runs, as it reads stake and NAV then.
    for after_the_end in [0, 1] {
        let mut h = rotating(1_000_000_000);
        let staking = h.engine.staking().unwrap();
        let end = staking.config.epoch_end(staking.config.epochs_elapsed(h.ts) + 8);
        let effective_at = end + after_the_end;
        elect(&mut h, 1, CANDIDATE, key(), effective_at);
        h.advance_to(end - 1);
        finalised(&mut h, effective_at);
        stake(&mut h, STAKER_A, 1);
        assert_eq!(h.engine.operator().map(|operator| operator.address), Some(operator().address));
        assert_eq!(h.engine.operator_candidate(), None);

        h.advance_to(effective_at);
        let events = h.cmd(Command::Tick);
        assert_eq!(h.engine.operator().map(|operator| operator.address), Some(key()));
        assert_eq!(share(&events), Some((CANDIDATE, UsdcUnits(200_000_000))));
        assert_eq!(leaf(&h, operator().address), 0);
    }
}

#[test]
fn a_lagging_mirror_keeps_the_share_with_the_outgoing_operator() {
    let mut h = rotating(1_000_000_000);
    let effective_at = h.ts + NOTICE_MS;
    elect(&mut h, 1, CANDIDATE, key(), effective_at);
    // Past the time, with the finalised frontier short of it: neither key nor share has moved.
    h.advance_to(effective_at + 1);
    finalised(&mut h, effective_at - 1);
    stake(&mut h, STAKER_A, 1);
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(operator().address));
    assert_eq!(h.engine.operator_candidate(), None);
    let events = close(&mut h);
    assert_eq!(share(&events), Some((operator().address, UsdcUnits(200_000_000))));
    assert_eq!(leaf(&h, CANDIDATE), 0);
}

#[test]
fn the_share_follows_each_election_as_it_takes_effect() {
    let mut h = rotating(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, key());
    stake(&mut h, STAKER_A, 1);
    assert_eq!(share(&close(&mut h)), Some((CANDIDATE, UsdcUnits(200_000_000))));

    // A successor waiting to take effect earns nothing yet, and the candidate in effect keeps it.
    let effective_at = h.ts + NOTICE_MS + 3 * DAY_MS;
    elect(&mut h, 2, SECOND, Client::new(0x67).account, effective_at);
    earn(&mut h);
    let waiting = pays(&mut h, CANDIDATE);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));

    h.advance_to(effective_at);
    finalised(&mut h, effective_at);
    earn(&mut h);
    assert_eq!(h.engine.operator_candidate(), Some(SECOND));
    let seated = pays(&mut h, SECOND);
    assert_eq!(leaf(&h, SECOND), seated);
    assert_eq!(leaf(&h, CANDIDATE), 200_000_000 + waiting);
}

#[test]
fn an_election_naming_the_key_in_effect_moves_the_candidate_at_its_effective_time() {
    // The operator in effect registered a candidacy under its own key: the key never moves, but the
    // candidate does, when the election takes effect.
    let mut h = rotating(1_000_000_000);
    stake(&mut h, STAKER_A, 1);
    let effective_at = h.ts + NOTICE_MS + 2 * DAY_MS;
    elect(&mut h, 1, CANDIDATE, operator().address, effective_at);
    assert_eq!(h.engine.operator_bounds().rotation, None);
    assert_eq!(share(&close(&mut h)), Some((operator().address, UsdcUnits(200_000_000))));
    assert_eq!(h.engine.operator_candidate(), None);

    // The mirror reaching the time first moves nothing while the sequenced time is short of it.
    finalised(&mut h, effective_at);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), None);

    h.advance_to(effective_at);
    earn(&mut h);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
    pays(&mut h, CANDIDATE);
}

#[test]
fn an_election_naming_the_key_in_effect_waits_for_the_mirror_too() {
    let mut h = rotating(1_000_000_000);
    let effective_at = h.ts + NOTICE_MS;
    elect(&mut h, 1, CANDIDATE, operator().address, effective_at);
    h.advance_to(effective_at + 1);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), None);
    finalised(&mut h, effective_at);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
}

#[test]
fn a_signed_rotation_hands_the_share_to_the_address_of_its_key() {
    let mut h = rotating(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, key());
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
    assert!(matches!(switch(&mut h, false)[..], [Event::RotationSwitched { enabled: false, .. }]));

    // With rotation off the elected key signs a rotation to a key no election names; the candidate
    // in effect stops earning from the moment that key does.
    let signed = Client::new(0x68).account;
    let events = h.operate_as(KEY_SEED, AdminCommand::RotateOperator { address: signed });
    assert_eq!(events.last(), Some(&Event::AdminApplied));
    h.advance(NOTICE_MS);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(signed));
    assert_eq!(h.engine.operator_candidate(), None);

    switch(&mut h, true);
    stake(&mut h, STAKER_A, 1);
    assert_eq!(share(&close(&mut h)), Some((signed, UsdcUnits(200_000_000))));
    assert_eq!(leaf(&h, CANDIDATE), 0);
}

#[test]
fn a_signed_rotation_back_to_the_elected_key_restores_its_candidate() {
    let mut h = rotating(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, key());
    h.cmd(Command::Tick);
    switch(&mut h, false);
    let signed = Client::new(0x68).account;
    h.operate_as(KEY_SEED, AdminCommand::RotateOperator { address: signed });
    h.advance(NOTICE_MS);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), None);
    h.operate_as(0x68, AdminCommand::RotateOperator { address: key() });
    h.advance(NOTICE_MS);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
}

#[test]
fn a_candidate_earns_again_when_its_key_returns_after_a_cancelled_successor() {
    let mut h = rotating(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, key());
    h.cmd(Command::Tick);
    // A successor is cancelled, so the last election is one that never took effect.
    let effective_at = h.ts + NOTICE_MS;
    elect(&mut h, 2, SECOND, Client::new(0x67).account, effective_at);
    let (block, chain_ref) = (h.log.len() as u64, "0xcancel".to_owned());
    h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: true, block, chain_ref });
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));

    // The key leaves by a signed rotation, and comes back by another.
    let signed = Client::new(0x68).account;
    h.operate_as(KEY_SEED, AdminCommand::RotateOperator { address: signed });
    h.advance(NOTICE_MS);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), None);
    h.operate_as(0x68, AdminCommand::RotateOperator { address: key() });
    h.advance(NOTICE_MS);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));

    switch(&mut h, true);
    stake(&mut h, STAKER_A, 1);
    assert_eq!(share(&close(&mut h)), Some((CANDIDATE, UsdcUnits(200_000_000))));
}

#[test]
fn before_v47_a_successor_keeps_no_election_naming_the_key_in_effect() {
    // The rule that keeps one from `V47` would change an earlier log's root.
    let mut h = at(ProtocolVersion::V46, 1_000_000_000);
    switch(&mut h, true);
    let effective_at = h.ts + NOTICE_MS + 2 * DAY_MS;
    elect(&mut h, 1, CANDIDATE, operator().address, effective_at);
    elect(&mut h, 2, SECOND, Client::new(0x67).account, effective_at + DAY_MS);
    assert_eq!(h.engine.operator_election().prior, None);
    let (block, chain_ref) = (h.log.len() as u64, "0xcancel".to_owned());
    h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: true, block, chain_ref });
    let last = h.engine.operator_election().last.expect("the successor stays the last");
    assert_eq!((last.candidate, last.cancelled), (SECOND, true));
}

#[test]
fn cancelling_a_successor_restores_the_election_it_superseded_with_or_without_a_handover() {
    for first_key in [operator().address, key()] {
        let mut h = rotating(1_000_000_000);
        let effective_at = h.ts + NOTICE_MS + 2 * DAY_MS;
        elect(&mut h, 1, CANDIDATE, first_key, effective_at);
        // A catch-up mirrors the successor before the first has taken effect, and the timelock
        // then cancels the successor: the registry names the first again.
        elect(&mut h, 2, SECOND, Client::new(0x67).account, effective_at + DAY_MS);
        assert_eq!(h.engine.operator_election().prior.map(|prior| prior.candidate), Some(CANDIDATE));
        let (block, chain_ref) = (h.log.len() as u64, "0xcancel".to_owned());
        h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: true, block, chain_ref });
        assert_eq!(h.engine.operator_election().last.map(|last| last.candidate), Some(CANDIDATE));
        switch(&mut h, true);

        h.advance_to(effective_at);
        finalised(&mut h, effective_at);
        stake(&mut h, STAKER_A, 1);
        assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE), "{first_key}");
        assert_eq!(share(&close(&mut h)), Some((CANDIDATE, UsdcUnits(200_000_000))), "{first_key}");
    }
}

#[test]
fn a_cancelled_election_is_never_kept_as_prior() {
    // E1 under a key other than the one in effect is cancelled before its time; rotation on again,
    // E2 is elected before E1's time and cancelled too. The cancelled E1 is not kept as `prior`
    // when E2 supersedes it, so cancelling E2 restores nothing: no handover waits on E1's time,
    // and the key in effect, and the share's recipient, stay past it.
    let mut h = rotating(1_000_000_000);
    let e1_at = h.ts + NOTICE_MS + 5 * DAY_MS;
    elect(&mut h, 1, CANDIDATE, key(), e1_at);
    let (block, chain_ref) = (h.log.len() as u64, "0xcancel1".to_owned());
    h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: true, block, chain_ref });
    switch(&mut h, true);

    let e2_at = h.ts + NOTICE_MS + DAY_MS;
    elect(&mut h, 2, SECOND, Client::new(0x67).account, e2_at);
    assert_eq!(h.engine.operator_election().prior, None);
    let (block, chain_ref) = (h.log.len() as u64, "0xcancel2".to_owned());
    h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: true, block, chain_ref });
    let last = h.engine.operator_election().last.expect("the cancelled successor stays the last");
    assert_eq!((last.candidate, last.cancelled), (SECOND, true));
    assert_eq!(h.engine.operator_bounds().rotation, None);

    // Past E1's time, with the mirror's frontier at it, no restored handover applies and the key
    // stays with the operator in effect.
    h.advance_to(e1_at);
    finalised(&mut h, e1_at);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(operator().address));
    assert_eq!(h.engine.operator_bounds().rotation, None);

    switch(&mut h, true);
    stake(&mut h, STAKER_A, 1);
    assert_eq!(share(&close(&mut h)), Some((operator().address, UsdcUnits(200_000_000))));
    assert_eq!(leaf(&h, CANDIDATE), 0);
}

#[test]
fn a_successor_keeps_nothing_of_an_election_that_has_taken_effect() {
    // Taken effect under the key already in effect, its candidate stays when the successor is
    // cancelled, with nothing to restore.
    let mut h = rotating(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, operator().address);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
    let effective_at = h.ts + NOTICE_MS;
    elect(&mut h, 2, SECOND, Client::new(0x67).account, effective_at);
    assert_eq!(h.engine.operator_election().prior, None);
    let (block, chain_ref) = (h.log.len() as u64, "0xcancel".to_owned());
    h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: true, block, chain_ref });
    assert_eq!(h.engine.operator_candidate(), Some(CANDIDATE));
    assert_eq!(h.engine.operator_bounds().rotation, None);

    // Nor does an election a signed rotation has replaced come back: its key does not.
    let mut h = rotating(1_000_000_000);
    seat(&mut h, 1, CANDIDATE, key());
    h.cmd(Command::Tick);
    switch(&mut h, false);
    let signed = Client::new(0x68).account;
    h.operate_as(KEY_SEED, AdminCommand::RotateOperator { address: signed });
    h.advance(NOTICE_MS);
    h.cmd(Command::Tick);
    switch(&mut h, true);
    let effective_at = h.ts + NOTICE_MS;
    elect(&mut h, 2, SECOND, Client::new(0x67).account, effective_at);
    assert_eq!(h.engine.operator_election().prior, None);
    let (block, chain_ref) = (h.log.len() as u64, "0xcancel".to_owned());
    h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: true, block, chain_ref });
    assert_eq!(h.engine.operator_bounds().rotation, None);
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(signed));
}

#[test]
fn a_cancelled_election_never_earns() {
    let mut h = rotating(1_000_000_000);
    let effective_at = h.ts + NOTICE_MS + 2 * DAY_MS;
    elect(&mut h, 1, CANDIDATE, key(), effective_at);
    let (block, chain_ref) = (h.log.len() as u64, "0xcancel".to_owned());
    h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: true, block, chain_ref });
    switch(&mut h, true);
    h.advance_to(effective_at);
    finalised(&mut h, effective_at);
    stake(&mut h, STAKER_A, 1);
    assert_eq!(h.engine.operator_candidate(), None);
    assert_eq!(share(&close(&mut h)), Some((operator().address, UsdcUnits(200_000_000))));
    assert_eq!(leaf(&h, CANDIDATE), 0);
}
