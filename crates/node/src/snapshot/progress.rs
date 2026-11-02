//! The replay facts a snapshot carries across its checkpoint.

use engine_core::protocol::ProtocolVersion;
use serde::{Deserialize, Serialize};

use crate::sequencer::GenesisProgress;

/// Replay facts carried across a checkpoint; every field is authenticated with the engine bytes.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Progress {
    pub genesis: GenesisProgress,
    pub activations: Vec<(ProtocolVersion, u64)>,
    pub checkpoints: u64,
    /// The seq of the latest applied `SetParams`; absent in a snapshot written before it was
    /// carried, and before any is applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params_seq: Option<u64>,
}
