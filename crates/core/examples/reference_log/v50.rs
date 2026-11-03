//! The session's `V50` part: the stress parameters and a listed market's financing markup move only
//! after a seven-day notice.

use engine_core::command::AdminCommand;
use engine_core::event::Event;
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice, StressSettings};
use engine_core::protocol::ProtocolVersion;

use crate::common::{BTC, Harness};
use crate::v48::INSTALLED_SEED;

fn sign(h: &mut Harness, command: AdminCommand) -> Vec<Event> {
    h.operate_as(INSTALLED_SEED, command)
}

fn refused(events: &[Event]) {
    assert!(matches!(events.last(), Some(Event::AdminRejected { .. })), "{events:?}");
}

fn applied(events: &[Event]) {
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
}

/// `V50` activates. A wider top stress level and a higher BTC markup are each refused without a
/// notice; each is announced for seven days on, refused a millisecond before then, and applies then,
/// spending its notice.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V50 });
    let mut params = h.engine.params().expect("the session sets parameters").clone();
    let top = params.stress_levels.len() - 1;
    params.stress_levels[top].min_spread_bps += 1;
    let mut relisted = h.engine.markets()[&BTC].spec.clone();
    relisted.financing.markup_bps += 25;
    refused(&sign(h, AdminCommand::SetParams(params.clone())));
    refused(&sign(h, AdminCommand::UpsertMarket(relisted.clone())));

    let due = h.ts + NOTICE_MS;
    let stress = Some(StressSettings::of(&params));
    let notice = ParamsNotice { profit_fee_bps: None, closeout_level_bps: None, stress, effective_at: due };
    applied(&sign(h, AdminCommand::AnnounceParams(notice)));
    let markup_bps = relisted.financing.markup_bps;
    applied(&sign(h, AdminCommand::AnnounceMarkup { market: BTC, markup_bps, effective_at: due }));
    h.advance_to(due - 1);
    refused(&sign(h, AdminCommand::SetParams(params.clone())));
    refused(&sign(h, AdminCommand::UpsertMarket(relisted.clone())));
    h.advance_to(due);
    applied(&sign(h, AdminCommand::SetParams(params)));
    applied(&sign(h, AdminCommand::UpsertMarket(relisted)));
    let bounds = h.engine.operator_bounds();
    assert!(bounds.notice.is_none() && bounds.markups.is_empty(), "{bounds:?}");
}
