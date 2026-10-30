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
    pub(crate) approval_registry: Option<crate::checkpoints::ApprovalRegistry>,
    pub(crate) checkpoint_parent: crate::log::CheckpointParent,
    pub(crate) checkpoints: Option<mpsc::Sender<crate::checkpoints::Work>>,
    /// The commands the config's genesis sequences, which the genesis the log ran must match (see
    /// `GenesisProgress::ensure_matches`).
    pub genesis: Vec<Command>,
    pub(crate) history_sealing: Option<crate::snapshot::sealing::Endpoint>,
    /// Told whether each checkpoint attempt signed (see `Gate::signing`).
    pub(crate) checkpoint_signing: Option<watch::Sender<bool>>,
}
