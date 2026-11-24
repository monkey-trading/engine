//! The reason a rejected price command is counted under.

/// A price rejection's reason, read from the engine's text. Tests sequence each rejection through
/// the engine, so a changed text fails them rather than falling into `other`.
pub(super) fn prices_reject_reason(reason: &str) -> &'static str {
    if reason == "unknown market" {
        "unknown_market"
    } else if reason.starts_with("malformed quote") {
        "malformed_quote"
    } else if reason.ends_with("ahead of the sequenced time") {
        "quote_ahead"
    } else if reason.ends_with("outside one open session") {
        "market_closed"
    } else if reason.ends_with("which the market does not take") {
        "contract_not_taken"
    } else {
        "other"
    }
}
