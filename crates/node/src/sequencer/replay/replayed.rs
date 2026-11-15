//! What a replay of a log yields (`Replay::finish`).

use std::collections::{BTreeMap, BTreeSet};

use engine_core::deposits::LegacyDeposit;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, MarketId, Millis};
use engine_core::{Engine, Fixed};

use crate::checkpoints::Work;
use crate::checkpoints::accounting::Accounting;
use crate::log;
use crate::sequencer::{GenesisProgress, SignedWithdrawal};
use crate::snapshot::RecoveryBase;

/// The result of replaying a log.
pub struct Replayed {
    /// Successful legacy EVM credits, derived from verified commands, never a host cursor.
    pub legacy_deposits: BTreeMap<String, LegacyDeposit>,
    pub(crate) legacy_approvals: crate::sequencer::legacy::Approvals,
    pub(crate) origin: crate::sequencer::origin::Origin,
    pub engine: Engine,
    pub(crate) accounting: Accounting,
    pub commands: u64,
    pub(crate) command_prefix: [u8; 32],
    /// The replay-authenticated prefix ring the log-head poster adopts against.
    pub(crate) prefix_ring: crate::log::PrefixRing,
    /// The log's `TurnTaken` commands and every checkpoint's root, which the boundary's log check
    /// attributes ranges by and restores its verified head against.
    pub(crate) takings: crate::log_check::Takings,
    pub(crate) checkpoint_roots: BTreeMap<u64, engine_core::eip712::B256>,
    pub checkpoints: u64,
    pub(crate) recovery_work: Vec<Work>,
    pub(crate) recovery_anchor_work: Option<Work>,
    pub(crate) last_signed_checkpoint: Option<crate::log::Checkpoint>,
    /// Where `last_signed_checkpoint` sits in the log.
    pub(crate) last_checkpoint_offset: Option<log::Offset>,
    /// The seq of the latest applied `SetParams`, if the replay saw one or its snapshot carried one.
    pub(crate) params_seq: Option<u64>,
    pub(crate) recovery_base: Option<RecoveryBase>,
    /// Withdrawals the last command approved that the log ends before signing, as a crash between
    /// a command's entry and its withdrawal entry leaves: `(account, amount, nonce)`.
    pub unsigned: Vec<(AccountId, Fixed, u64)>,
    /// Durable approvals the engine holds that no withdrawal entry has signed. Only a no-gate node
    /// leaves these: a crash can sit between the approval's checkpoint sync and its signature sync.
    pub unreleased: Vec<engine_core::withdrawal::WithdrawalApproval>,
    /// Each protocol version the log activated, with the seq it took effect at.
    pub activations: Vec<(ProtocolVersion, u64)>,
    /// How far the log got through genesis.
    pub genesis: GenesisProgress,
    /// Sequenced time of the command the last checkpoint covers.
    pub last_checkpoint_ts: Option<Millis>,
    /// Approvals whose custody checkpoints precede their signature release.
    pub deferred: Vec<(u64, AccountId, Fixed, u64)>,
    /// Cumulative approved amount; the public log alone cannot establish which nonces custody paid.
    pub unpaid: Fixed,
    /// Named checkpoint signers, for comparison with independently trusted attestations.
    pub checkpoint_signers: BTreeSet<AccountId>,
    /// Every approval with its event index, in FIFO order.
    pub(crate) payout_order: Vec<((AccountId, u64), usize)>,
    /// The latest authorization each signed approval holds. A recovery from a checkpoint snapshot
    /// sees only the suffix after it.
    pub(crate) authorized: BTreeMap<(AccountId, u64), SignedWithdrawal>,
    pub(crate) requires_checkpoint_gate: bool,
    /// Whether the log ends inside an access-source failure episode.
    pub(crate) access_failed: bool,
    /// The last mark sampled per market, so the recovered sequencer continues change-only
    /// reporting exactly where the replay left off. Empty without a marks sink.
    pub last_marks: BTreeMap<MarketId, Fixed>,
    /// The cumulative command-prefix digest the replay captured at the posted head's seq, for the
    /// startup check. Checkpoints do not reset it, so a posted head binds every command's sealed
    /// bytes, not only the state since the last checkpoint.
    pub link_at: Option<engine_core::eip712::B256>,
    /// The sequenced time of the command at the posted head's seq: where the stall alarm starts.
    pub ts_at: Option<Millis>,
    /// The sequenced time of the command the vault-finalized checkpoint covers, where the replay or
    /// its snapshot reached it, or, with `[log_heads]`, the snapshot's prefix pass did for one under
    /// its base with no head posted: the stall alarm's anchor while no head of this key is posted.
    pub anchor_ts: Option<Millis>,
}
