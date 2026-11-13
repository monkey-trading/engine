//! Why a command was refused, as events carry it.

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::access::AccessReject;
use crate::price::PriceError;
use crate::types::{MarketId, Millis};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reject {
    Access {
        reason: AccessReject,
    },
    /// The node cannot yet pay an on-chain withdrawal; no collateral is debited.
    PayoutUnavailable,
    UnknownMarket,
    UnknownOrder,
    InvalidOrder {
        reason: String,
    },
    MarketClosed,
    ReduceOnly,
    /// The market has no index price. `error` is nested because `PriceError` carries its own `kind`
    /// tag.
    NoPrice {
        error: PriceError,
    },
    /// The market is quoted in another currency than USD, and its FX market has no price to convert
    /// at.
    NoFxPrice {
        fx_market: MarketId,
    },
    /// The live price is further from the client's quote than allowed.
    Slippage {
        quoted: Fixed,
        live: Fixed,
        slippage_bps: i64,
    },
    InsufficientMargin {
        equity: Fixed,
        required: Fixed,
    },
    InsufficientFunds {
        available: Fixed,
    },
    /// From `V34`, a withdrawal of whole USDC below `minimum`.
    BelowMinimumWithdrawal {
        minimum: Fixed,
    },
    HouseCapacity,
    /// From `V40`, a reversal whose blended spread would exceed the market's close-spread ceiling.
    /// The pure close stays within the ceiling: a refused trader can close reduce-only and reopen.
    CloseSpreadCeiling {
        ceiling_bps: i64,
    },
    /// A limit or stop order from an account that already rests `max`, the parameters' cap.
    TooManyOrders {
        max: usize,
    },
    /// An amount at or below zero, or above `Fixed::MAX_INPUT`.
    InvalidAmount,
    /// A custody reference that was already credited.
    DuplicateReference,
    /// From `V25`, a stake or unstake before the staking config is initialised.
    StakingNotInitialised,
    /// From `V25`, an unstake of more MONKEY than the account has staked.
    InsufficientStake,
    /// EVM credits under receipt finality require an exact custody receipt.
    CustodyReceiptRequired,
    /// An unsigned request, where the rules in force require a signature.
    MissingSignature,
    /// A signature that is not the account's over this request, or one the rules in force cannot
    /// check.
    InvalidSignature,
    /// A nonce other than the account's next: already used, or ahead of it.
    InvalidNonce {
        expected: u64,
    },
    /// A signed request sequenced after its expiry.
    Expired {
        expires_at: Millis,
    },
    /// A signed request whose expiry lies more than `MAX_EXPIRY_AHEAD_MS` past the sequenced time.
    ExpiryTooFar {
        latest: Millis,
    },
    /// From `V41`, a registration naming a key that cannot be a session key: the account's own, the
    /// recorded operator's, or, for a revocation, a key the account never registered.
    InvalidSessionKey,
    /// From `V41`, a request signed by a session key the account revoked.
    SessionKeyRevoked,
    /// From `V41`, a request signed by a session key past its registration's expiry.
    SessionKeyExpired {
        expires_at: Millis,
    },
    /// From `V41`, a funds request signed by a live session key: session keys place and cancel
    /// orders only, and can never move collateral.
    SessionKeyScope,
    /// From `V41`, a registration or revocation sequenced before `V41` activated.
    SessionKeyUnsupported,
    /// From `V41`, a registration that would hold more than `max` live session keys, or, with `max`
    /// the introduction budget's capacity, introduce a new address with the account's budget
    /// empty (the lifetime cap before `V42`); a registration prunes the account's expired and
    /// revoked keys first.
    TooManySessionKeys {
        max: usize,
    },
    /// A batch request (cancel-all or close-all) sequenced before `V44` activated.
    BatchUnsupported,
}
