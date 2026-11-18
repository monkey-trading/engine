//! The operator's status notice, from `V78`: one short plain-text message, such as "Security
//! maintenance: withdrawals are paused", that every node serves to clients until the operator
//! clears it. It is state, set and cleared by sequenced admin commands, so every member of a
//! rotation serves the same notice, a handover carries it with the log, and a replay rebuilds it.
//! It moves no value, so the operator in office may send it whether or not votes govern.

use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::protocol::ProtocolVersion;
use crate::types::Millis;

/// The most characters a notice may hold.
pub const MAX_NOTICE_CHARS: usize = 280;

/// What makes text markup or a link, which a notice may not hold.
const FORBIDDEN: [char; 5] = ['<', '>', '&', '[', ']'];
/// Substrings, compared in ASCII lower case, that make text a link.
const LINK_MARKERS: [&str; 3] = ["://", "www.", "mailto:"];
/// Characters outside the control category that break a line or render nothing, which a notice may not
/// hold: the line and paragraph separators, every default-ignorable code point (soft hyphen, fillers,
/// variation selectors, zero-width and bidirectional marks and overrides, invisible operators, the byte
/// order mark, tag characters) and the interlinear annotation marks.
const INVISIBLE: [RangeInclusive<char>; 17] = [
    '\u{00ad}'..='\u{00ad}',
    '\u{034f}'..='\u{034f}',
    '\u{061c}'..='\u{061c}',
    '\u{115f}'..='\u{1160}',
    '\u{17b4}'..='\u{17b5}',
    '\u{180b}'..='\u{180f}',
    '\u{200b}'..='\u{200f}',
    '\u{2028}'..='\u{202e}',
    '\u{2060}'..='\u{206f}',
    '\u{3164}'..='\u{3164}',
    '\u{fe00}'..='\u{fe0f}',
    '\u{feff}'..='\u{feff}',
    '\u{ffa0}'..='\u{ffa0}',
    '\u{fff0}'..='\u{fffb}',
    '\u{1bca0}'..='\u{1bca3}',
    '\u{1d173}'..='\u{1d17a}',
    '\u{e0000}'..='\u{e0fff}',
];

/// The notice in force and the sequenced time it was set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub text: String,
    pub set_at_ms: Millis,
}

/// Checks `text` as a notice.
/// Error cases:
/// - empty, blank, or with leading or trailing whitespace
/// - more than `MAX_NOTICE_CHARS` characters
/// - a control character, such as a line break, or an invisible one (`INVISIBLE`), such as a line
///   separator or a zero-width space
/// - `<`, `>`, `&`, `[` or `]`, which markup uses
/// - a link: `://`, `www.` or `mailto:`
pub fn check_notice_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("the notice is empty".into());
    }
    if text.trim() != text {
        return Err("the notice starts or ends with whitespace".into());
    }
    let length = text.chars().count();
    if length > MAX_NOTICE_CHARS {
        return Err(format!("the notice is {length} characters, over the {MAX_NOTICE_CHARS} allowed"));
    }
    if text.chars().any(|c| c.is_control() || INVISIBLE.iter().any(|range| range.contains(&c))) {
        return Err("the notice holds a control or invisible character".into());
    }
    if let Some(markup) = text.chars().find(|c| FORBIDDEN.contains(c)) {
        return Err(format!("the notice holds {markup:?}, which markup uses"));
    }
    let lower = text.to_ascii_lowercase();
    if let Some(link) = LINK_MARKERS.iter().find(|marker| lower.contains(**marker)) {
        return Err(format!("the notice holds {link:?}, which makes a link"));
    }
    Ok(())
}

impl Engine {
    /// The notice in force, if any.
    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    /// Sets the notice, replacing any in force, at the sequenced time.
    /// Error cases:
    /// - before `V78`
    /// - text `check_notice_text` refuses
    pub(crate) fn set_notice(&mut self, text: &str) -> Result<(), String> {
        self.check_notice_version()?;
        check_notice_text(text)?;
        self.notice = Some(Notice { text: text.to_owned(), set_at_ms: self.now });
        Ok(())
    }

    /// Clears the notice.
    /// Error cases:
    /// - before `V78`
    /// - no notice is in force
    pub(crate) fn clear_notice(&mut self) -> Result<(), String> {
        self.check_notice_version()?;
        self.notice.take().map(drop).ok_or_else(|| "no notice is in force".to_owned())
    }

    fn check_notice_version(&self) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V78 {
            return Err(format!("a notice needs protocol {:?}", ProtocolVersion::V78));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
