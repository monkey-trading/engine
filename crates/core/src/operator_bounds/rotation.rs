//! The operator key's rotation, from `V35`: a signed `RotateOperator` takes effect seven days after
//! it is logged, and from then the old key signs nothing.

use crate::engine::Engine;
use crate::operator_bounds::{NOTICE_MS, PendingRotation};
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, Millis};

impl Engine {
    /// Records a rotation to `address`, replacing any pending one, from seven days after now.
    /// Error cases:
    /// - before `V35`, or with no operator recorded
    /// - from `V37`, while rotation is on, when only an election moves the key
    /// - from `V37`, while an uncancelled elected handover waits to take effect, even with rotation
    ///   off: only that handover moves the key until it applies or the registry cancels it
    /// - from `V48`, while an installed handover waits to take effect, even with rotation off: only
    ///   that handover moves the key until it applies
    /// - the zero address, or the address already operating
    pub(crate) fn rotate_operator(&mut self, address: AccountId) -> Result<(), String> {
        self.require_bounds_protocol("an operator rotation")?;
        if self.rotation_by_election() {
            return Err("while rotation is on, only an election moves the operator key".into());
        }
        let operator = self.operator.ok_or("no operator is recorded")?;
        if address == AccountId([0; 20]) || address == operator.address {
            return Err("an operator rotation names a new, nonzero address".into());
        }
        if self.operator_bounds.rotation.is_some_and(|rotation| self.is_elected(rotation)) {
            return Err("while an elected handover waits, only it moves the operator key".into());
        }
        if self.protocol_version >= ProtocolVersion::V48
            && self.operator_bounds.rotation.is_some_and(|rotation| self.is_installed(rotation))
        {
            return Err("while an installed handover waits, only it moves the operator key".into());
        }
        self.operator_bounds.rotation = Some(PendingRotation { address, effective_at: self.now + NOTICE_MS });
        Ok(())
    }

    /// Hands the operator's role to the pending rotation's address once it is due (see
    /// `Engine::due_rotation`), before the command stamped `ts` is authorised, so the old key signs
    /// nothing from then.
    pub(crate) fn apply_due_rotation(&mut self, ts: Millis) {
        let Some(rotation) = self.due_rotation(ts) else {
            return;
        };
        // A rotation is recorded only beside a recorded operator, which is never removed.
        self.operator.as_mut().expect("a rotation needs a recorded operator").address = rotation.address;
        self.operator_bounds.rotation = None;
        self.operator_bounds.rotations += 1;
    }
}
