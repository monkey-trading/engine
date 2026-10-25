//! The reference session's helpers: requests it sends and how it reads events back.

use std::fs::OpenOptions;
use std::io::{BufWriter, ErrorKind, Write};
use std::path::Path;

use anyhow::anyhow;
use engine_core::command::{OrderKind, OrderRequest};
use engine_core::event::Event;
use engine_core::types::{AccountId, MarketId, OrderId, Side};

use crate::common::reference::ReferenceEntry;
use crate::common::{BTC, f};

/// A reduce-only market order quoted at `quoted`.
pub(super) fn reducing(account: AccountId, market: MarketId, side: Side, qty: &str, quoted: &str) -> OrderRequest {
    OrderRequest {
        account,
        client_id: 8,
        market,
        side,
        qty: f(qty),
        kind: OrderKind::Market { quoted: f(quoted), max_slippage_bps: None },
        reduce_only: true,
        auth: None,
    }
}

/// A 0.01 BTC market buy for `account`, quoted at the session's first BTC mid, unsigned.
pub(super) fn small_buy(account: AccountId) -> OrderRequest {
    OrderRequest {
        account,
        client_id: 9,
        market: BTC,
        side: Side::Buy,
        qty: f("0.01"),
        kind: OrderKind::Market { quoted: f("100000.123456789"), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    }
}

/// The order id an `OrderAccepted` among `events` carries; panics if there is none.
pub(super) fn accepted(events: &[Event]) -> OrderId {
    events
        .iter()
        .find_map(|e| match e {
            Event::OrderAccepted { order_id, .. } => Some(*order_id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected an accepted order, got {events:?}"))
}

/// The event's `type` tag, plus the rejection kind where there is one.
pub(super) fn event_kind(event: &Event) -> String {
    let json = serde_json::to_value(event).expect("events serialise");
    let kind = json["type"].as_str().unwrap_or_default().to_owned();
    match json["reason"]["kind"].as_str() {
        Some(reason) => format!("{kind}:{reason}"),
        None => kind,
    }
}

/// Writes `entries` as JSON lines to a new file at `path`.
/// Error cases:
/// - a file already at `path`, which a committed reference log never overwrites
/// - the file cannot be written
pub(super) fn write_entries(path: &Path, entries: &[ReferenceEntry]) -> anyhow::Result<()> {
    let file = OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| match e.kind() {
        ErrorKind::AlreadyExists => {
            anyhow!("{} exists; a committed reference log is never regenerated", path.display())
        }
        _ => anyhow!(e).context(format!("creating {}", path.display())),
    })?;
    let mut out = BufWriter::new(file);
    for entry in entries {
        serde_json::to_writer(&mut out, entry)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}
