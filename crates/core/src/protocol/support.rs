//! What a rule set supports, for the code paths that ask of a version rather than compare it.

use crate::protocol::ProtocolVersion;

impl ProtocolVersion {
    /// The newest version this build implements.
    pub const LATEST: ProtocolVersion = ProtocolVersion::V90;

    /// Whether access screening applies; the standalone legacy V25 rules predate access.
    pub fn supports_access(self) -> bool {
        self == Self::V24 || self >= Self::V26
    }

    /// Whether a client request's screening evidence must name the request it answers, as before `V85`;
    /// from `V85` it may name none and cover the account. A deposit's evidence always names its own.
    pub fn binds_screening_to_request(self) -> bool {
        self < Self::V85
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
