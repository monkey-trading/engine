//! What bounds the loss trigger from `V59`: its activation, and the operator's and the votes'
//! changes to the stress parameters. The window and its slots are constants, so only a protocol
//! version changes them.

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::Engine;
use crate::params::StressLevel;
use crate::protocol::ProtocolVersion;

/// The level `V59`'s activation gives a loss threshold of zero, where the parameters name none.
pub const ACTIVATION_LEVEL: usize = 2;

/// A stress level's loss threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LossTrigger {
    pub level: usize,
    pub house_loss: Fixed,
}

/// The loss thresholds `levels` name, by level.
pub fn loss_triggers(levels: &[StressLevel]) -> Vec<LossTrigger> {
    levels
        .iter()
        .enumerate()
        .filter_map(|(level, stress)| {
            stress.trigger.as_ref()?.house_loss.map(|house_loss| LossTrigger { level, house_loss })
        })
        .collect()
}

impl Engine {
    /// Starts the trigger when an activation carries the log from before `V59` to `version` at or past
    /// it: the window opens at the trading result now (`Engine::open_window`), parameters that name no
    /// loss threshold and have a level `ACTIVATION_LEVEL` give its trigger one of zero, and the
    /// thresholds in force become launch's.
    pub(crate) fn start_house_loss(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V59 || version < ProtocolVersion::V59 {
            return;
        }
        self.open_window();
        let Some(params) = self.params.as_mut() else { return };
        if loss_triggers(&params.stress_levels).is_empty()
            && let Some(level) = params.stress_levels.get_mut(ACTIVATION_LEVEL)
        {
            level.trigger.get_or_insert_with(Default::default).house_loss = Some(Fixed::ZERO);
        }
        self.operator_bounds.launch_loss_triggers = Some(loss_triggers(&params.stress_levels));
    }

    /// From `V59`, records the loss thresholds in force as launch's with the first parameters set
    /// under it; its activation over parameters already set records them there (`start_house_loss`).
    pub(crate) fn record_launch_loss_triggers(&mut self) {
        if self.protocol_version >= ProtocolVersion::V59
            && self.operator_bounds.launch_loss_triggers.is_none()
            && let Some(params) = &self.params
        {
            self.operator_bounds.launch_loss_triggers = Some(loss_triggers(&params.stress_levels));
        }
    }

    /// From `V59`, checks stress levels a notice, the operator or a vote would set keep every launch
    /// loss threshold: a level at or above its level whose threshold is at most its own.
    /// Error cases:
    /// - a launch threshold with no such level
    pub(crate) fn check_loss_triggers_kept(&self, levels: &[StressLevel]) -> Result<(), String> {
        let launch = self.operator_bounds.launch_loss_triggers.as_deref().unwrap_or_default();
        let set = loss_triggers(levels);
        match launch.iter().find(|launched| {
            !set.iter().any(|kept| kept.level >= launched.level && kept.house_loss <= launched.house_loss)
        }) {
            Some(launched) => Err(format!(
                "the stress levels keep a house loss trigger at level {} or above, at most its launch threshold {}",
                launched.level, launched.house_loss
            )),
            None => Ok(()),
        }
    }
}
