//! What a rule set supports, for the code paths that ask of a version rather than compare it.

use super::ProtocolVersion;

impl ProtocolVersion {
    /// Whether access screening applies; the standalone legacy V25 rules predate access.
    pub fn supports_access(self) -> bool {
        self == Self::V24 || self >= Self::V26
    }

    /// Whether this rule set credits exact receipts with withdrawal principal locks.
    pub fn supports_deposit_finality(self) -> bool {
        self >= ProtocolVersion::V20
    }

    /// Whether this is `V2`, which engine state leaves out of its encoding.
    pub fn is_v2(&self) -> bool {
        *self == ProtocolVersion::V2
    }
}
