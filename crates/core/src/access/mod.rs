//! Recorded access policy, signed client eligibility, and screening inputs for V24.

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::auth::RequestAuth;
use crate::eip712::{B256, Signature};
use crate::price::MAX_QUOTE_LEAD_MS;
use crate::types::{AccountId, Millis};

mod engine;

/// How far ahead of the sequenced time source evidence may be stamped: the host clock that stamps
/// commands may lag the source's within trusted time's drift, as it may lag a venue's.
pub const MAX_EVIDENCE_LEAD_MS: Millis = MAX_QUOTE_LEAD_MS;

/// A client's declaration, without additional qualification claims.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EligibilityClass {
    Professional,
    EligibleClient,
}

impl EligibilityClass {
    /// The complete statement shown to and signed by this client.
    pub fn statement(self) -> &'static str {
        match self {
            Self::Professional => "I attest that I am a professional client.",
            Self::EligibleClient => "I attest that I am an eligible client.",
        }
    }
}

/// A declaration always requires a signature, including in unsigned development logs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EligibilityRequest {
    pub account: AccountId,
    pub class: EligibilityClass,
    pub auth: RequestAuth,
}

/// The source identity and freshness bound recorded once for the lifetime of a log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessPolicy {
    pub source: AccountId,
    pub max_age_ms: Millis,
}

/// The configured source classifies jurisdictions; the engine contains no country list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Jurisdiction {
    Allowed,
    UnitedStates,
    Sanctioned,
    Unknown,
}

/// Unknown screening results never mean clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SanctionsStatus {
    Clear,
    Sanctioned,
    Unknown,
}

/// Public screening inputs contain no IP address, endpoint, credential, or personal details.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screening {
    pub source: AccountId,
    pub request_hash: B256,
    pub observed_at: Millis,
    pub account: Option<AccountId>,
    pub payer: Option<AccountId>,
    pub jurisdiction: Jurisdiction,
    pub account_status: SanctionsStatus,
    pub payer_status: SanctionsStatus,
    pub signature: Signature,
}

/// A bounded source failure is logged as a typed reason, without transport error details.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFailure {
    NotConfigured,
    MissingPeer,
    Unavailable,
    InvalidResponse,
}

/// The actual source result used for one action, including failures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccessEvidence {
    Screened(Screening),
    Failed { reason: SourceFailure },
}

/// A final deposit screened at the custody boundary, including the actual funding address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessDeposit {
    /// `None` credits house capital.
    pub account: Option<AccountId>,
    pub payer: Option<AccountId>,
    pub amount: Fixed,
    pub chain_ref: String,
}

/// No held deposit has an automatic release or refund path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldDeposit {
    pub deposit: AccessDeposit,
    pub reason: AccessReject,
}

/// Deterministic access refusal reasons, carried in the public log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessReject {
    ProtocolInactive,
    PolicyMissing,
    EvidenceMissing,
    SourceNotConfigured,
    PeerUnknown,
    SourceUnavailable,
    SourceInvalid,
    EvidenceMismatch,
    EvidenceStale,
    UnitedStates,
    SanctionedJurisdiction,
    JurisdictionUnknown,
    AccountSanctioned,
    AccountUnknown,
    PayerSanctioned,
    PayerUnknown,
    /// The deposit names no funding address, as legacy vault events do, so it can never be screened.
    PayerMissing,
    EligibilityRequired,
    AlreadyAttested,
    DepositHeld,
    DepositPending,
}

impl Screening {
    /// The sanctions refusal this screening gives for the subjects an action checks: a confirmed
    /// sanction of either before an unknown status of either, so an unclassified subject never hides
    /// a sanctioned one.
    pub fn sanctions_refusal(&self, account: bool, payer: bool) -> Option<AccessReject> {
        let subjects = [
            (account, self.account_status, AccessReject::AccountSanctioned, AccessReject::AccountUnknown),
            (payer, self.payer_status, AccessReject::PayerSanctioned, AccessReject::PayerUnknown),
        ];
        let checked = || subjects.iter().filter(|(checked, ..)| *checked);
        checked()
            .find_map(|(_, status, sanctioned, _)| (*status == SanctionsStatus::Sanctioned).then_some(*sanctioned))
            .or_else(|| {
                checked().find_map(|(_, status, _, unknown)| (*status == SanctionsStatus::Unknown).then_some(*unknown))
            })
    }
}

impl AccessReject {
    /// Whether the refusal holds a deposit for good: a confirmed sanction of its account or payer, or
    /// no payer to screen. Every other refusal can be cured by a declaration, a classification, or a
    /// working source, so the deposit stays pending and is retried.
    pub fn holds_deposit(self) -> bool {
        matches!(self, Self::AccountSanctioned | Self::PayerSanctioned | Self::PayerMissing)
    }

    /// Whether the refusal had no verified source determination: the source failed, was absent, or
    /// gave evidence that did not verify.
    pub fn is_source_failure(self) -> bool {
        matches!(
            self,
            Self::PolicyMissing
                | Self::EvidenceMissing
                | Self::SourceNotConfigured
                | Self::PeerUnknown
                | Self::SourceUnavailable
                | Self::SourceInvalid
                | Self::EvidenceMismatch
                | Self::EvidenceStale
        )
    }
}
