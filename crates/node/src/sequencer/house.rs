//! What `GET /v1/house/state` answers from memory: the protocol parameters with the seq that set
//! them, and the latest checkpoint the log holds. The sequencer keeps the seq and the checkpoint
//! as it logs them, recovery restores both from the replay it already runs, and the parameters
//! themselves are the engine's.

use engine_core::Engine;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::params::ProtocolParams;
use serde::Serialize;
use tokio::sync::oneshot;

use crate::log::{Checkpoint, Offset};
use crate::sequencer::{StreamBody, StreamMsg};

/// The `SetParams` `cmd` carries, as an unsigned or a signed admin command.
pub(crate) fn set_params_of(cmd: &Command) -> Option<&ProtocolParams> {
    let admin = match cmd {
        Command::Admin(admin) => admin,
        Command::SignedAdmin(request) => &request.command,
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

/// The answer: the seq it was read at, the parameters (`None` before the first `SetParams`), and the
/// latest checkpoint in the stream's form (`None` before the first).
#[derive(Clone, Debug, Serialize)]
pub struct HouseState {
    pub seq: Option<u64>,
    pub params: Option<ParamsAt>,
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

    /// Answers every waiting read from `engine`, which holds the flushed state, and `params_seq`,
    /// the seq of the `SetParams` that set its parameters.
    pub(super) fn answer(&mut self, engine: &Engine, params_seq: Option<u64>) {
        if self.waiting.is_empty() {
            return;
        }
        let params = engine.params().map(|params| ParamsAt { seq: params_seq, params: params.clone() });
        let state = HouseState { seq: engine.last_seq(), params, checkpoint: self.checkpoint.clone() };
        for reply in self.waiting.drain(..) {
            let _ = reply.send(state.clone());
        }
    }
}
