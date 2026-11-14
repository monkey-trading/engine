//! When votes govern: rotation as the registry's `rotationEnabled()` has it, the drawn turns that
//! name the operator, and from `V59` the set in effect.

use crate::command::AdminCommand;
use crate::engine::Engine;
use crate::event::Event;
use crate::governance::is_governed;
use crate::protocol::ProtocolVersion;
use crate::turns::boundary;
use crate::types::Millis;

impl Engine {
    /// From `V56`, whether only a vote may apply `command`: while votes govern, a governed command, a
    /// notice the vote's own notice replaces, or the legacy custody activation.
    pub fn refused_outside_vote(&self, command: &AdminCommand) -> bool {
        self.governed_at(command, self.now)
    }

    /// Whether only a vote may apply `command` stamped at `at` under the rules in force, as
    /// `refused_outside_vote` says at the sequenced time: how the node decides, at a start's stamp,
    /// what it leaves to a vote.
    pub fn governed_at(&self, command: &AdminCommand, at: Millis) -> bool {
        self.protocol_version >= ProtocolVersion::V56
            && self.votes_govern(at)
            && (is_governed(command)
                || matches!(
                    command,
                    AdminCommand::AnnounceParams(_)
                        | AdminCommand::AnnounceMarkup { .. }
                        | AdminCommand::AnnounceListing { .. }
                        | AdminCommand::ActivateCustodyFinality { .. }
                ))
    }

    /// Whether votes govern at `at`: rotation is on as the registry's `rotationEnabled()` has it, its
    /// switch or a drawn turn naming the operator, the engine's own or one it refused, and from `V59`
    /// the rotation set in effect has members who can propose.
    pub(super) fn votes_govern(&self, at: Millis) -> bool {
        if self.protocol_version < ProtocolVersion::V59 {
            return self.rotation_by_election() || self.governance.turns_drawn;
        }
        self.rotation_on(at) && self.daily_turns.set_at(at).is_some_and(|set| !set.members.is_empty())
    }

    /// From `V59`, whether the registry's `rotationEnabled()` holds at `at`: its switch, a drawn turn that
    /// names the operator by then (`Governance::turns_from`), or, until a mirrored frontier carries the
    /// registry's turn state, a turn the activation could not see (`Governance::turns_drawn`).
    pub(super) fn rotation_on(&self, at: Millis) -> bool {
        self.operator_election.rotation_enabled
            || self.governance.turns_drawn
            || self.governance.turns_from.is_some_and(|from| from <= at)
    }

    /// From `V56`, records a mirrored turn assignment the engine credited, accepted or refused, as a drawn
    /// turn, from `V59` naming the operator from its boundary, and an installation, which ends the drawn
    /// turns, as none.
    pub(crate) fn note_turns_drawn(&mut self, events: &[Event]) {
        if self.protocol_version < ProtocolVersion::V56 {
            return;
        }
        for event in events {
            match event {
                Event::TurnAssigned { turn, .. } | Event::TurnRefused { turn, .. }
                    if self.protocol_version >= ProtocolVersion::V59 =>
                {
                    // `assign_turn` credits only a turn with a boundary.
                    let start = boundary(*turn).expect("a credited turn has a boundary");
                    self.governance.turns_from = Some(self.governance.turns_from.map_or(start, |from| from.min(start)));
                }
                Event::TurnAssigned { .. } | Event::TurnRefused { .. } => self.governance.turns_drawn = true,
                Event::OperatorInstalled { .. } => {
                    self.governance.turns_drawn = false;
                    self.governance.turns_from = None;
                }
                _ => {}
            }
        }
    }

    /// At `V59`'s activation, by an `ActivateProtocol` to `version`, records when a drawn turn first
    /// names the operator from what the log holds: the earliest boundary of an accepted turn in effect
    /// or waiting to apply. A turn refused before `V59` left no boundary, so while the log holds rotation
    /// sets a turn may name the operator unseen: rotation counts as on until the next mirrored frontier
    /// carries the registry's turn state (`staking_progress`).
    pub(crate) fn start_turns_from(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V59 || version < ProtocolVersion::V59 {
            return;
        }
        let turns = &self.daily_turns;
        self.governance.turns_from =
            turns.in_effect.iter().chain(turns.assigned.values()).filter_map(|turn| boundary(turn.turn)).min();
        self.governance.turns_drawn = !turns.sets.is_empty();
    }
}
