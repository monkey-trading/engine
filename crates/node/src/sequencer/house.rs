//! What `GET /v1/house/state` answers from memory: the protocol parameters with the seq that set
//! them, the pending parameter and listing notices, from `V56` the vote contract with the passed
//! changes waiting and the restrictions in force, any close-spread ceiling adoption waiting to take
//! effect, from `V59` the house's loss over the trailing day, and the latest checkpoint the log holds. The sequencer keeps the seq and the checkpoint
//! as it logs them, recovery restores both from the replay it already runs, and the parameters
//! themselves are the engine's.

use std::collections::BTreeMap;

use engine_core::Engine;
use engine_core::close_spread::PendingCeiling;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::governance::Governance;
use engine_core::house_loss::HouseLossReading;
use engine_core::listing_bounds::ListingNotice;
use engine_core::operator_bounds::ParamsNotice;
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::toxicity::{SCORE_RULES, ScoreRules};
use engine_core::types::MarketId;
use serde::Serialize;
use tokio::sync::oneshot;

use crate::log::{Checkpoint, Offset};
use crate::sequencer::{StreamBody, StreamMsg};

/// The `SetParams` `cmd` carries, as an unsigned or a signed admin command, or a passed change.
pub(crate) fn set_params_of(cmd: &Command) -> Option<&ProtocolParams> {
    let admin = match cmd {
        Command::Admin(admin) => admin,
        Command::SignedAdmin(request) => &request.command,
        Command::ApplyProposal(voted) => &voted.command,
        _ => return None,
    };
    match admin {
        AdminCommand::SetParams(params) => Some(params),
        _ => None,
    }
}

/// Whether `cmd`, which produced `events`, replaced the protocol parameters.
pub(crate) fn applied_set_params(cmd: &Command, events: &[Event]) -> bool {
    set_params_of(cmd).is_some() && events.contains(&Event::AdminApplied)
}

/// Records `sequenced`'s seq in `params_seq` when it replaced the protocol parameters.
pub(super) fn note_params(params_seq: &mut Option<u64>, sequenced: &Sequenced, events: &[Event]) {
    if applied_set_params(&sequenced.cmd, events) {
        *params_seq = Some(sequenced.seq);
    }
}

/// The parameters in force and the seq of the `SetParams` that set them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ParamsAt {
    /// `None` only when the parameters come from a snapshot written before it carried the seq and
    /// no `SetParams` has applied since.
    pub seq: Option<u64>,
    #[serde(flatten)]
    pub params: ProtocolParams,
}

/// The answer: the seq it was read at, the parameters in force (`None` before the first `SetParams`),
/// the pending parameter notice (`None` while none waits), from `V54` the toxicity score's protocol
/// constants, each listed market's pending listing notice (a markup, and from `V52` its spread and
/// margin bands, in the market read's units), the mirrored votes (`None` before any is recorded: the
/// vote contract, each passed change with the time it applies from, each restriction in force with
/// its end, and the baseline of each field one holds), and the latest checkpoint in the stream's
/// form (`None` before the first). The bands in force are the market read's.
#[derive(Clone, Debug, Serialize)]
pub struct HouseState {
    pub seq: Option<u64>,
    pub params: Option<ParamsAt>,
    pub params_notice: Option<ParamsNotice>,
    pub listing_notices: BTreeMap<MarketId, ListingNotice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score_rules: Option<ScoreRules>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub governance: Option<Governance>,
    /// A close-spread ceiling adoption waiting to take effect, signed or voted, with its time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_ceiling: Option<PendingCeiling>,
    /// From `V59`, the house's marked loss to its clients over the trailing day, the time the window
    /// starts, and the stress level the loss sets in every market.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub house_loss: Option<HouseLossReading>,
    pub checkpoint: Option<StreamMsg>,
}

/// The latest checkpoint, which the engine does not hold, and the reads waiting for the batch's
/// flush. The seq of the latest `SetParams` is `Progress`'s, which a snapshot carries.
#[derive(Default)]
pub(super) struct Latest {
    checkpoint: Option<StreamMsg>,
    waiting: Vec<oneshot::Sender<HouseState>>,
}

impl Latest {
    /// The checkpoint a recovery replay ended on, with its place in the log.
    pub(super) fn recovered(checkpoint: Option<(Offset, Checkpoint)>) -> Self {
        let checkpoint = checkpoint.map(|(offset, cp)| StreamMsg { offset, body: StreamBody::Checkpoint(cp) });
        Self { checkpoint, waiting: Vec::new() }
    }

    /// Notes the checkpoint just logged at `offset`.
    pub(super) fn checkpointed(&mut self, offset: Offset, checkpoint: &Checkpoint) {
        self.checkpoint = Some(StreamMsg { offset, body: StreamBody::Checkpoint(checkpoint.clone()) });
    }

    /// Holds a read until the batch is flushed, so no answer shows what a crash could take back.
    pub(super) fn wait(&mut self, reply: oneshot::Sender<HouseState>) {
        self.waiting.push(reply);
    }

    /// Takes every waiting read with the answer from `engine`, which holds the state they were
    /// waiting for, and `params_seq`, the seq of the `SetParams` that set its parameters: they are
    /// sent once the batch is durable.
    pub(super) fn take_answer(
        &mut self,
        engine: &Engine,
        params_seq: Option<u64>,
    ) -> Option<(Vec<oneshot::Sender<HouseState>>, HouseState)> {
        if self.waiting.is_empty() {
            return None;
        }
        let params = engine.params().map(|params| ParamsAt { seq: params_seq, params: params.clone() });
        let (params_notice, listing_notices) =
            (engine.operator_bounds().notice.clone(), engine.operator_bounds().markups.clone());
        let score_rules = (engine.protocol_version() >= ProtocolVersion::V54).then_some(SCORE_RULES);
        let governance = Some(engine.governance()).filter(|governance| !governance.is_default()).cloned();
        let (seq, pending_ceiling, checkpoint) =
            (engine.last_seq(), engine.pending_ceiling().cloned(), self.checkpoint.clone());
        let house_loss = engine.house_loss();
        let state = HouseState {
            seq,
            params,
            params_notice,
            score_rules,
            listing_notices,
            governance,
            pending_ceiling,
            house_loss,
            checkpoint,
        };
        Some((std::mem::take(&mut self.waiting), state))
    }
}
