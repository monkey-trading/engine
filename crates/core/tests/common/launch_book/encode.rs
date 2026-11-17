//! The book's encoding, assembled as text: an account's key and one `"key":value` entry at a time.

use engine_core::types::AccountId;

/// An account's key in the encoding's maps.
pub(super) fn key_of(account: AccountId) -> String {
    serde_json::to_string(&account).expect("an address encodes")
}

/// Appends `"key":value` to a JSON object under construction.
pub(super) fn entry(out: &mut String, key: &str, value: &str) {
    if out.len() > 1 {
        out.push(',');
    }
    out.push_str(key);
    out.push(':');
    out.push_str(value);
}
