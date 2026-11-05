//! A key handover's final checkpoint, and its checkpoint-gate admission.

use anyhow::anyhow;

use super::{Gate, HandoverGate};
use crate::sequencer::Sequencer;

impl Sequencer {
    /// Attempts the checkpoint a key handover hands over as the final one. The gate's verdict owns
    /// the stop: refused while it would withhold or defer the checkpoint, so the node refuses the
    /// handover and goes on sequencing. While a detected log rollback freezes sequencing no
    /// checkpoint is written at all: the handover is refused with the freeze as its reason.
    /// Error cases:
    /// - the log cannot be written
    pub(crate) fn handover_checkpoint(&mut self) -> anyhow::Result<HandoverGate> {
        if self.frozen() {
            return Ok(HandoverGate::Refused(anyhow!(crate::sequencer::HandoverRefused::new(
                "sequencing is frozen after a detected log rollback: no final checkpoint is written".into(),
            ))));
        }
        let Some(seq) = self.engine.last_seq() else {
            return Ok(HandoverGate::Refused(anyhow!(crate::sequencer::HandoverRefused::new(
                "no command has applied: there is no checkpoint to hand over".into(),
            ))));
        };
        self.checkpoint()?;
        if self.last_checkpoint == Some(seq) {
            return Ok(HandoverGate::Admitted);
        }
        let causes = self.gate.as_ref().map_or_else(|| "no checkpoint was written".to_string(), Gate::withhold_causes);
        Ok(HandoverGate::Refused(anyhow!(crate::sequencer::HandoverRefused::new(format!(
            "the final checkpoint is withheld ({causes}): the handover is refused and this node goes on \
                 sequencing until the checkpoint_withheld alert clears"
        ),))))
    }
}
