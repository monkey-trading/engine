//! The session's `V78` part: the operator's status notice. A vote activates the version. While votes
//! govern, the operator in office sets a notice, which one refused for its text leaves in force, replaces it, and
//! clears it; a second clear is refused.

use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::notice::{MAX_NOTICE_CHARS, Notice};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;

use crate::v56::pass;
use crate::v76::office_seed;

/// Activates `V78` by vote; then the notice described above.
pub(crate) fn session(h: &mut crate::common::Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V78 };
    let events = pass(h, 210, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 210, command: activation })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V78);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);

    let text = "Security maintenance: withdrawals are paused.";
    let events = h.operate_as(office_seed(h), AdminCommand::SetNotice { text: text.into() });
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.notice(), Some(&Notice { text: text.into(), set_at_ms: h.ts }));

    // Text a notice may not hold is refused and leaves the notice in force.
    for refused in ["", "see https://example.com", &"a".repeat(MAX_NOTICE_CHARS + 1)] {
        let events = h.operate_as(office_seed(h), AdminCommand::SetNotice { text: refused.into() });
        assert!(
            matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("the notice")),
            "{events:?}"
        );
    }
    assert_eq!(h.engine.notice().map(|notice| notice.text.as_str()), Some(text));

    h.advance(60_000);
    let resumed = "Withdrawals have resumed.";
    let events = h.operate_as(office_seed(h), AdminCommand::SetNotice { text: resumed.into() });
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.notice(), Some(&Notice { text: resumed.into(), set_at_ms: h.ts }));

    let events = h.operate_as(office_seed(h), AdminCommand::ClearNotice);
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.notice(), None);
    let events = h.operate_as(office_seed(h), AdminCommand::ClearNotice);
    assert!(
        matches!(events.last(), Some(Event::AdminRejected { reason }) if reason == "no notice is in force"),
        "{events:?}"
    );
}
