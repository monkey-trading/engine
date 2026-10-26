//! Commands the engine must accept before anything else: the client auth, the operator, protocol
//! activation, and genesis. Like any command, they wait for a stamp trusted time admits.

use anyhow::bail;
use engine_core::Engine;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::LegacyDeposit;
use engine_core::params::NotionalWeighting;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Millis;
use tracing::{info, warn};

use crate::config::Genesis;
use crate::sequencer::Sequencer;
use crate::sequencer::genesis::{GENESIS_REF, check_genesis, genesis_commands, rejection};

impl Sequencer {
    /// Requires a signed publisher snapshot before catch-up or ordinary sequencing.
    /// Error cases:
    /// - a scheduled legacy log needs activation prerequisites that would charge rollovers first
    pub fn wait_for_benchmarks(&mut self) -> anyhow::Result<()> {
        let missing = if self.engine.client_auth().is_none() {
            Some("client auth")
        } else if self.engine.operator().is_none() {
            Some("operator")
        } else if self.engine.params().is_some_and(|params| params.toxicity.notional.is_none()) {
            Some("toxicity notional weighting")
        } else {
            None
        };
        if self.engine.next_rollover().is_some()
            && let Some(prerequisite) = missing
        {
            bail!(
                "benchmark synchronization requires recorded {prerequisite}; legacy logs need prerequisite migration before configuring publishers"
            );
        }
        self.benchmark_sync_enabled = true;
        self.benchmark_sync = true;
        warn!(alert = "benchmark_sync_pending", "waiting for signed publisher history before catch-up");
        Ok(())
    }

    /// Requires another publisher attempt if refused time resumes over an unpaid rollover.
    pub(super) fn note_benchmark_time_admission(&mut self, admitted: bool, ts: Millis) {
        if !self.benchmark_sync_enabled {
            return;
        }
        if !admitted {
            self.benchmark_time_refused = true;
        } else if std::mem::take(&mut self.benchmark_time_refused)
            && self.engine.next_rollover().is_some_and(|next| next <= ts)
            && !self.benchmark_sync
        {
            self.benchmark_sync = true;
            warn!(alert = "benchmark_sync_pending", "waiting for signed publisher history after trusted time recovers");
        }
    }

    /// The seq of the last command in the log, if any.
    pub fn last_seq(&self) -> Option<u64> {
        self.engine.last_seq()
    }

    /// Whether genesis has yet to run: its last command, the house capital credit, is not in the
    /// log. Recovery refuses a log that started genesis without closing it, so such a log holds no
    /// more than protocol activation.
    pub fn needs_genesis(&self) -> bool {
        !self.engine.is_credited(GENESIS_REF)
    }

    /// Records the config's client auth and operator and activates the latest protocol version,
    /// whichever the log lacks, then checkpoints. All run at the log's next seq, so earlier commands
    /// keep the rules they ran under, once trusted time admits a stamp (see `sequence_required`).
    /// `recover` has already refused a log that records another client auth or operator.
    /// Error cases:
    /// - the engine rejects any of them
    /// - the log cannot be written
    pub fn activate_protocol(&mut self) -> anyhow::Result<()> {
        let legacy = self.legacy_deposits.values().cloned().collect();
        let mut cmds = activation_commands(&self.engine, &self.client_auth, self.operator, legacy);
        if self.engine.access_policy().is_none()
            && let Some(policy) = &self.access_policy
        {
            cmds.push(Command::Admin(AdminCommand::SetAccessPolicy(policy.clone())));
        }
        if cmds.is_empty() {
            return Ok(());
        }
        info!(version = ?ProtocolVersion::LATEST, seq = self.next_seq, "activating protocol");
        self.sequence_required("protocol activation", cmds)
    }

    /// As `activate_protocol`, activating `version` instead of the latest, for tests of earlier rules.
    /// Error cases:
    /// - the engine rejects any of them
    /// - the log cannot be written
    #[cfg(test)]
    pub(crate) fn activate_protocol_at(&mut self, version: ProtocolVersion) -> anyhow::Result<()> {
        let current = self.engine.protocol_version();
        let legacy = self.legacy_deposits.values().cloned().collect();
        let mut cmds = activation_commands(&self.engine, &self.client_auth, self.operator, legacy);
        let activates = |cmd: &Command| {
            matches!(
                cmd,
                Command::Admin(AdminCommand::ActivateProtocol { .. } | AdminCommand::ActivateCustodyFinality { .. })
            )
        };
        cmds.retain(|cmd| !activates(cmd) || current < version);
        for cmd in &mut cmds {
            if let Command::Admin(admin) = cmd
                && activates(&Command::Admin(admin.clone()))
            {
                let legacy_deposits = match admin {
                    AdminCommand::ActivateCustodyFinality { legacy_deposits, .. } => std::mem::take(legacy_deposits),
                    _ => Vec::new(),
                };
                *admin = if version.supports_deposit_finality() && !self.engine.deposit_finality_enabled() {
                    AdminCommand::ActivateCustodyFinality { version, legacy_deposits }
                } else {
                    AdminCommand::ActivateProtocol { version }
                };
            }
        }
        if cmds.is_empty() {
            return Ok(());
        }
        self.sequence_required("protocol activation", cmds)
    }

    /// Gives parameters the log set before `V11`, which do not weigh toxicity markouts by notional,
    /// the config's `weighting`, ahead of `activate_protocol`: `V11` refuses to activate over
    /// parameters without it. The command is the node's own, so the engine takes it unsigned however
    /// much is credited. It waits for a stamp trusted time admits, like activation.
    /// Error cases:
    /// - the engine rejects the weighting
    /// - the log cannot be written
    pub fn carry_toxicity_notional(&mut self, weighting: NotionalWeighting) -> anyhow::Result<()> {
        if self.engine.params().is_none_or(|params| params.toxicity.notional.is_some()) {
            return Ok(());
        }
        info!(prior = %weighting.prior, memory = %weighting.memory, "carrying the config's toxicity notional weighting");
        self.sequence_required("toxicity notional", vec![Command::Admin(AdminCommand::SetToxicityNotional(weighting))])
    }

    /// Checks that the engine accepts every genesis command, then sequences them as ordinary admin
    /// commands and checkpoints, once trusted time admits a stamp (see `sequence_required`). A
    /// genesis the engine would reject queues and writes nothing. The check runs under the latest
    /// rules, so callers activate the protocol first, as `run` does.
    /// Error cases:
    /// - the engine would reject a genesis command (see `check_genesis`)
    /// - the engine rejects one when it is sequenced
    /// - the log cannot be written
    pub fn genesis(&mut self, genesis: &Genesis) -> anyhow::Result<()> {
        check_genesis(&self.client_auth, self.operator, genesis)?;
        self.sequence_required("genesis", genesis_commands(genesis))
    }

    /// Queues commands the engine must accept, `what` they are for, and sequences everything queued
    /// if trusted time admits a stamp now. Otherwise they wait, like any command, and run ahead of
    /// the first stamp it admits: nothing, at start, runs on the host clock alone.
    /// Error cases:
    /// - the engine rejects any queued command
    /// - the log cannot be written
    pub(super) fn sequence_required(&mut self, what: &'static str, cmds: Vec<Command>) -> anyhow::Result<()> {
        self.required.push((what, cmds));
        self.sequence_queued_if_admitted()?;
        self.flush()
    }

    /// Sequences the queued required commands if trusted time admits a stamp now.
    /// Error cases:
    /// - the engine rejects any of them
    /// - the log cannot be written
    pub(super) fn sequence_queued_if_admitted(&mut self) -> anyhow::Result<()> {
        if self.required.is_empty() {
            return Ok(());
        }
        match self.admitted_ts() {
            Some(ts) => self.sequence_queued(ts),
            None => Ok(()),
        }
    }

    /// Sequences the queued required commands at `ts`, checkpointing after each group.
    /// Error cases:
    /// - the engine rejects any of them
    /// - the log cannot be written
    pub(super) fn sequence_queued(&mut self, ts: Millis) -> anyhow::Result<()> {
        let mut queued = std::mem::take(&mut self.required).into_iter();
        while let Some((what, cmds)) = queued.next() {
            info!(what, commands = cmds.len(), "sequencing required commands");
            let mut cmds = cmds.into_iter();
            while let Some(cmd) = cmds.next() {
                let activation = cmd.is_dated_rate_activation();
                if self.benchmark_sync && self.engine.next_rollover().is_some_and(|next| next <= ts) && !activation {
                    self.required.push((what, std::iter::once(cmd).chain(cmds).collect()));
                    self.required.extend(queued);
                    return Ok(());
                }
                let events = self.stamp(ts, cmd)?;
                if let Some(reason) = rejection(&events) {
                    bail!("{what}: rejected: {reason}");
                }
            }
            self.checkpoint()?;
        }
        Ok(())
    }
}

/// The commands that record `client_auth` and `operator` and activate the latest protocol version,
/// whichever `engine` lacks.
pub(super) fn activation_commands(
    engine: &Engine,
    client_auth: &ClientAuth,
    operator: Operator,
    legacy_deposits: Vec<LegacyDeposit>,
) -> Vec<Command> {
    activation_commands_for(engine, client_auth, operator, ProtocolVersion::LATEST, legacy_deposits)
}

/// Builds activation for an explicit ruleset; production always selects LATEST.
fn activation_commands_for(
    engine: &Engine,
    client_auth: &ClientAuth,
    operator: Operator,
    version: ProtocolVersion,
    legacy_deposits: Vec<LegacyDeposit>,
) -> Vec<Command> {
    let mut cmds = Vec::new();
    if engine.client_auth().is_none() {
        cmds.push(Command::Admin(AdminCommand::SetClientAuth(client_auth.clone())));
    }
    if engine.operator().is_none() {
        cmds.push(Command::Admin(AdminCommand::SetOperator(operator)));
    }
    if engine.protocol_version() < version {
        let command = if version.supports_deposit_finality() && !engine.deposit_finality_enabled() {
            AdminCommand::ActivateCustodyFinality { version, legacy_deposits }
        } else {
            AdminCommand::ActivateProtocol { version }
        };
        cmds.push(Command::Admin(command));
    }
    cmds
}

#[cfg(test)]
impl Sequencer {
    /// Builds historical unsigned/auth/storage fixtures under the V22 rules, the last before access screening.
    /// Error cases:
    /// - required historical commands cannot be sequenced or written
    pub(crate) fn activate_legacy_protocol(&mut self) -> anyhow::Result<()> {
        let legacy = self.legacy_deposits.values().cloned().collect();
        let cmds =
            activation_commands_for(&self.engine, &self.client_auth, self.operator, ProtocolVersion::V22, legacy);
        if cmds.is_empty() {
            return Ok(());
        }
        self.sequence_required("V22 historical fixture", cmds)
    }
}
