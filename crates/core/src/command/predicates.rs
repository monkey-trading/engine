//! What kind of command a [`Command`] or [`AdminCommand`] is: who may sign it, and which
//! activations and snapshots it carries.

use crate::command::{AdminCommand, Command};
use crate::protocol::ProtocolVersion;

impl AdminCommand {
    /// Whether this is a dated publisher snapshot.
    pub fn is_benchmark_snapshot(&self) -> bool {
        matches!(self, Self::SetBenchmarkRates(_))
    }

    /// Whether the operator may sign this command: listings, parameters, benchmark rates, stress
    /// floors, reduce-only, the emission schedule, corporate actions, the staking config, and from `V35`
    /// parameter notices and operator rotations, from `V49` markup notices, and from `V52` listing
    /// notices. The
    /// node alone sequences the others, from its config and build.
    pub fn is_operator_command(&self) -> bool {
        match self {
            AdminCommand::SetParams(_)
            | AdminCommand::UpsertMarket(_)
            | AdminCommand::SetBenchmarkRate { .. }
            | AdminCommand::SetBenchmarkRates(_)
            | AdminCommand::SetStressFloor { .. }
            | AdminCommand::SetReduceOnly { .. }
            | AdminCommand::InitEmissions(_)
            | AdminCommand::ScheduleCorporateAction(_)
            | AdminCommand::InitStaking(_)
            | AdminCommand::AnnounceParams(_)
            | AdminCommand::AnnounceMarkup { .. }
            | AdminCommand::AnnounceListing { .. }
            | AdminCommand::RotateOperator { .. }
            | AdminCommand::AdoptCloseSpreadCeiling { .. } => true,
            AdminCommand::SetClientAuth(_)
            | AdminCommand::SetAccessPolicy(_)
            | AdminCommand::SetOperator(_)
            | AdminCommand::SetToxicityNotional(_)
            | AdminCommand::SetCloseSpreadCeilings { .. }
            | AdminCommand::SetGovernance(_)
            | AdminCommand::ActivateCustodyFinality { .. }
            | AdminCommand::ActivateProtocol { .. } => false,
        }
    }
}

impl Command {
    /// Targets the activation that validates staking cash reserves before closing an overdue epoch.
    pub(crate) fn changes_staking_reserves(&self, current: ProtocolVersion) -> bool {
        let admin = match self {
            Self::Admin(admin) => admin,
            Self::SignedAdmin(request) => &request.command,
            Self::ApplyProposal(voted) => &voted.command,
            _ => return false,
        };
        let target = match admin {
            AdminCommand::ActivateProtocol { version } | AdminCommand::ActivateCustodyFinality { version, .. } => {
                *version
            }
            _ => return false,
        };
        (current < ProtocolVersion::V28 && target >= ProtocolVersion::V28)
            || (current < ProtocolVersion::V29 && target >= ProtocolVersion::V29)
            || (current < ProtocolVersion::V30 && target >= ProtocolVersion::V30)
            || (current < ProtocolVersion::V31 && target >= ProtocolVersion::V31)
    }

    /// Whether this activates a protocol version, as the node sequences it or as a passed vote does.
    pub fn is_activation(&self) -> bool {
        let activates = |admin: &AdminCommand| matches!(admin, AdminCommand::ActivateProtocol { .. });
        matches!(self, Self::Admin(admin) if activates(admin))
            || matches!(self, Self::ApplyProposal(voted) if activates(&voted.command))
    }

    /// Whether this node-only activation must precede dated benchmark catch-up.
    pub fn is_dated_rate_activation(&self) -> bool {
        matches!(self,
            Self::Admin(AdminCommand::ActivateProtocol { version }
                | AdminCommand::ActivateCustodyFinality { version, .. }) if *version >= ProtocolVersion::V18)
    }

    /// Whether this carries an atomic dated benchmark snapshot, signed or during genesis.
    pub fn is_benchmark_batch(&self) -> bool {
        matches!(self, Self::Admin(AdminCommand::SetBenchmarkRates(_)))
            || matches!(self, Self::SignedAdmin(req) if matches!(req.command, AdminCommand::SetBenchmarkRates(_)))
    }
}
