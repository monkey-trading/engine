//! The sequencer's construction config.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::Command;
use engine_core::types::AccountId;
use tokio::sync::{mpsc, watch};

use crate::clock::Clock;
use crate::log::Manifest;
use crate::metrics::Metrics;

pub struct SequencerConfig<'a> {
    pub log_dir: &'a Path,
    pub segment_commands: u64,
    /// Where the log writer publishes its manifest.
    pub manifest: watch::Sender<Manifest>,
    pub checkpoint_every: u64,
    pub chain_id: u64,
    pub vault: AccountId,
    /// The clocks commands are stamped and checked with.
    pub clock: Arc<dyn Clock>,
    /// The most a stamp may differ from trusted time.
    pub max_drift: Duration,
    /// The client auth the config gives, which a log that records one must match.
    pub client_auth: ClientAuth,
    pub access_policy: Option<engine_core::access::AccessPolicy>,
    /// The operator the config gives, which a log that records one must match.
    pub operator: Operator,
    /// Where the sequencer records its metrics.
    pub metrics: Arc<Metrics>,
    pub(crate) checkpoint_registry: Option<crate::checkpoints::Registry>,
    /// How long a checkpoint may stay held back, with none becoming final, before `checkpoint_backlog`.
    pub(crate) checkpoint_stalled_after: Duration,
    pub(crate) approval_registry: Option<crate::checkpoints::ApprovalRegistry>,
    pub(crate) checkpoint_parent: crate::log::CheckpointParent,
    pub(crate) checkpoints: Option<mpsc::Sender<crate::checkpoints::Work>>,
    /// The commands the config's genesis sequences, which the genesis the log ran must match (see
    /// `GenesisProgress::ensure_matches`).
    pub genesis: Vec<Command>,
    /// The close-spread ceilings the config's genesis adopts, which an upgrade to `V40` carries
    /// into a legacy log before its activation (see `Sequencer::activate_protocol`).
    pub close_spread_ceilings: Vec<engine_core::close_spread::CloseSpreadCeiling>,
    pub(crate) history_sealing: Option<crate::snapshot::sealing::Endpoint>,
    /// Told whether each checkpoint attempt signed (see `Gate::signing`).
    pub(crate) checkpoint_signing: Option<watch::Sender<bool>>,
    /// The registry's operator in effect, while rotation is off, at the finalized block the
    /// checkpoint setup read: recovery admits a config naming its key before the log has mirrored
    /// its installation, and no benchmark hold waits on the key it replaced.
    pub(crate) registry_operator: Option<crate::checkpoints::RegistryOperator>,
    /// The on-chain head poster's publish end, when `[log_heads]` runs: the released-head watch
    /// and the rollback freeze latch the poster shares with the sequencer.
    pub(crate) log_heads: Option<crate::log_heads::Publisher>,
    /// This enclave's release, which takes each daily turn the log assigns the config's operator key
    /// with a `TurnTaken` before anything else in it; `None` where this node takes no turn.
    pub(crate) turn_release: Option<engine_core::eip712::B256>,
    /// With sealed keys, the generation each `TurnTaken` waits for them to be sealed in; `None` takes
    /// each turn as it applies.
    pub(crate) turn_sealing: Option<crate::sequencer::turns::TurnSealing>,
    /// The vote contract and restriction lifetime the config names, which the node records once `V56`
    /// applies (see `Sequencer::activate_protocol`); a log that records others is refused.
    pub(crate) governance: Option<engine_core::governance::GovernanceConfig>,
}
