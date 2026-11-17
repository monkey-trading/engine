//! The operator's status notice from `V78`, through `Engine::apply`: set and cleared by the operator's
//! signed commands, held in state with the sequenced time it was set, refused for text a notice may
//! not hold or before `V78`, and carried by a replay of the log.

mod common;

use common::{CHAIN_ID, Harness, VAULT, operator};
use engine_core::Engine;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::notice::{MAX_NOTICE_CHARS, Notice};
use engine_core::protocol::ProtocolVersion;

const TEXT: &str = "Security maintenance: withdrawals are paused.";

/// The operator, client auth and `version` recorded, and nothing else.
fn at(version: ProtocolVersion) -> Harness {
    let mut h = Harness::empty();
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }));
    h.admin(AdminCommand::SetOperator(operator()));
    h.admin(AdminCommand::ActivateProtocol { version });
    h
}

fn set(text: &str) -> AdminCommand {
    AdminCommand::SetNotice { text: text.into() }
}

fn rejected(reason: &str) -> Vec<Event> {
    vec![Event::AdminRejected { reason: reason.into() }]
}

#[test]
fn the_operator_sets_a_notice_with_the_time_and_replaces_it() {
    let mut h = at(ProtocolVersion::V78);
    assert_eq!(h.engine.notice(), None);
    assert_eq!(h.operate(set(TEXT)), [Event::AdminApplied]);
    let first = h.ts;
    assert_eq!(h.engine.notice(), Some(&Notice { text: TEXT.into(), set_at_ms: first }));

    h.advance(5_000);
    assert_eq!(h.operate(set("Withdrawals have resumed.")), [Event::AdminApplied]);
    assert_eq!(h.engine.notice(), Some(&Notice { text: "Withdrawals have resumed.".into(), set_at_ms: first + 5_000 }));
}

#[test]
fn clearing_removes_the_notice_and_a_second_clear_is_refused() {
    let mut h = at(ProtocolVersion::V78);
    h.operator_admin(set(TEXT));
    assert_eq!(h.operate(AdminCommand::ClearNotice), [Event::AdminApplied]);
    assert_eq!(h.engine.notice(), None);
    assert_eq!(h.operate(AdminCommand::ClearNotice), rejected("no notice is in force"));
}

#[test]
fn text_a_notice_may_not_hold_is_refused_and_changes_nothing() {
    let mut h = at(ProtocolVersion::V78);
    h.operator_admin(set(TEXT));
    let long = "a".repeat(MAX_NOTICE_CHARS + 1);
    let cases = [
        ("", "the notice is empty"),
        (long.as_str(), "the notice is 281 characters, over the 280 allowed"),
        ("a<b>c", "the notice holds '<', which markup uses"),
        ("see https://example.com", "the notice holds \"://\", which makes a link"),
        ("two\nlines", "the notice holds a control or invisible character"),
        ("two\u{2028}lines", "the notice holds a control or invisible character"),
    ];
    for (text, reason) in cases {
        assert_eq!(h.operate(set(text)), rejected(reason), "{text:?}");
    }
    assert_eq!(h.engine.notice().map(|notice| notice.text.as_str()), Some(TEXT));
}

#[test]
fn the_longest_notice_is_accepted() {
    let mut h = at(ProtocolVersion::V78);
    assert_eq!(h.operate(set(&"a".repeat(MAX_NOTICE_CHARS))), [Event::AdminApplied]);
}

#[test]
fn only_the_operators_signature_sets_a_notice() {
    let mut h = at(ProtocolVersion::V78);
    let events = h.operate_as(0x33, set(TEXT));
    assert!(matches!(events.last(), Some(Event::AdminRejected { .. })), "{events:?}");
    assert_eq!(h.engine.notice(), None);
    // The node alone sequences the unsigned form once an operator is recorded and money is credited.
    h.cmd(Command::HouseCapital { amount: h.house_capital, chain_ref: "genesis".into() });
    let events = h.cmd(Command::Admin(set(TEXT)));
    assert!(matches!(events.last(), Some(Event::AdminRejected { .. })), "{events:?}");
    assert_eq!(h.engine.notice(), None);
}

#[test]
fn a_notice_is_refused_before_v78() {
    let mut h = at(ProtocolVersion::V74);
    assert_eq!(h.operate(set(TEXT)), rejected("a notice needs protocol V78"));
    assert_eq!(h.operate(AdminCommand::ClearNotice), rejected("a notice needs protocol V78"));
    assert_eq!(h.engine.notice(), None);
}

#[test]
fn the_notice_is_in_the_state_root_and_a_replay_reproduces_it() {
    // Engines that differ only in the notice text, at the same seq, time and nonce, have different roots.
    let root_after = |text: &str| {
        let mut h = at(ProtocolVersion::V78);
        h.operator_admin(set(text));
        h.engine.state_root()
    };
    assert_ne!(root_after("One notice."), root_after("Another notice."));
    assert_eq!(root_after("One notice."), root_after("One notice."));

    let mut h = at(ProtocolVersion::V78);
    h.operator_admin(set(TEXT));
    h.advance(1_000);
    h.operator_admin(set("Another notice."));
    h.operator_admin(AdminCommand::ClearNotice);
    h.operator_admin(set(TEXT));
    let expected = h.engine.state_root();

    let mut replica = Engine::new();
    for sequenced in &h.log {
        replica.apply(sequenced).unwrap();
    }
    assert_eq!(replica.notice(), h.engine.notice());
    assert_eq!(replica.state_root(), expected);
}
