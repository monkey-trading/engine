//! Genesis: the `[genesis]` config as the commands that open a log, the check that the engine
//! accepts every one of them before any is written, and how far a replayed log got through them.
//! Genesis is all or nothing: recovery refuses a log that started genesis without reaching the
//! checkpoint after its house capital credit, one whose genesis no config loaded today can give, and
//! one whose genesis is not the config's.

use anyhow::{Context, bail};
use engine_core::Engine;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::params::NotionalWeighting;
use engine_core::protocol::ProtocolVersion;
use serde::{Deserialize, Serialize};

use crate::config::Genesis;
use crate::sequencer::required::activation_commands;

/// The custody reference genesis credits house capital under, as its last command.
pub(crate) const GENESIS_REF: &str = "genesis";
/// What an operator does about a log whose genesis did not apply in full, when nothing follows it.
const FRESH_LOG: &str = "with the node stopped, remove node.log_dir and history.dir to start a fresh log";
/// What an operator does when commands follow such a genesis: they may hold client funds.
const KEEP_LOG: &str = "commands follow genesis in the log, so keep it for an operator to examine";
/// How an operator replaces a log no config can resume when commands follow its genesis.
const MOVE_LOG: &str = "commands follow genesis in the log, so with the node stopped, move node.log_dir and \
                        history.dir aside for an operator to examine, and start a fresh log";

/// How far a replayed log got through genesis. Genesis starts at the first command `genesis_commands`
/// gives, ends with its house capital credit, and closes at the next checkpoint. It applies in full
/// when it closes as one run the engine accepted throughout.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GenesisProgress {
    started: Option<u64>,
    credited: bool,
    closed: bool,
    /// The first command from the start of genesis to its credit that the engine rejected, with its
    /// reason.
    rejected: Option<(u64, String)>,
    /// The seq of a second `SetParams` before the credit: genesis run again before its first run
    /// finished.
    restarted: Option<u64>,
    /// Whether any command follows the credit.
    followed: bool,
    /// The commands from the start of genesis to its credit, in order.
    commands: Vec<Command>,
    /// The toxicity notional weighting the node carried, after genesis, into parameters set before
    /// `V11` without one (see `Sequencer::carry_toxicity_notional`).
    carried: Option<NotionalWeighting>,
}

impl GenesisProgress {
    /// Takes the next replayed command and the events it produced.
    pub(super) fn command(&mut self, sequenced: &Sequenced, events: &[Event]) {
        let cmd = &sequenced.cmd;
        if self.credited {
            self.followed = true;
            if let Command::Admin(AdminCommand::SetToxicityNotional(weighting)) = cmd {
                self.carried.get_or_insert(*weighting);
            }
            return;
        }
        if self.started.is_none() && !is_genesis_command(cmd) {
            return;
        }
        if self.started.is_some() && matches!(cmd, Command::Admin(AdminCommand::SetParams(_))) {
            self.restarted.get_or_insert(sequenced.seq);
        }
        self.started.get_or_insert(sequenced.seq);
        self.commands.push(cmd.clone());
        match rejection(events) {
            Some(reason) => {
                self.rejected.get_or_insert_with(|| (sequenced.seq, reason.to_owned()));
            }
            None => self.credited = is_genesis_credit(cmd),
        }
    }

    /// Takes the next checkpoint.
    pub(super) fn checkpoint(&mut self) {
        self.closed |= self.credited;
    }

    /// Refuses a log whose genesis started but did not apply in full. The error says to start a fresh
    /// log only when no command follows genesis.
    /// Error cases:
    /// - the engine rejected a command from the start of genesis to its house capital credit
    /// - genesis ran again before its first run reached the credit
    /// - genesis started, but the log ends before the checkpoint after its house capital credit
    pub fn ensure_closed(&self) -> anyhow::Result<()> {
        let flaw = if let Some((seq, reason)) = &self.rejected {
            format!("genesis did not apply in full: the engine rejected seq {seq} ({reason})")
        } else if let Some(seq) = self.restarted {
            format!("genesis did not apply in full: it ran again from seq {seq} before its first run finished")
        } else if let (Some(seq), false) = (self.started, self.closed) {
            format!("genesis started at seq {seq} but did not reach the checkpoint after its house capital credit")
        } else {
            return Ok(());
        };
        bail!("{flaw}; {}", if self.followed { KEEP_LOG } else { FRESH_LOG })
    }

    /// Refuses a log whose genesis is not `genesis`, the commands the config's genesis sequences, so
    /// the config hash the node attests names the genesis its engine runs under. The log's genesis is
    /// compared in the form a config gives it under the latest protocol (see `restated`): parameters
    /// set before `V11` take the config's toxicity notional weighting, or, once the node has carried
    /// one into the log, that one. A genesis that form leaves one config load refuses, under rules
    /// newer than the log, is refused first: no config can name it, so the log needs replacing.
    /// Error cases:
    /// - genesis started, and config load under `client_auth` and `operator` would refuse it (see
    ///   `check_genesis`)
    /// - genesis started, and its commands differ from `genesis`
    pub fn ensure_matches(
        &self,
        genesis: &[Command],
        client_auth: &ClientAuth,
        operator: Operator,
    ) -> anyhow::Result<()> {
        if self.started.is_none() {
            return Ok(());
        }
        let config_weighting = match genesis.first() {
            Some(Command::Admin(AdminCommand::SetParams(config))) => config.toxicity.notional,
            _ => None,
        };
        let ran = self.restated(self.carried.or(config_weighting));
        if let Err(refused) = check_genesis_commands(client_auth, operator, ran.clone()) {
            let replace = if self.followed { MOVE_LOG } else { FRESH_LOG };
            bail!(
                "no current config can name this log's genesis: config load under protocol {:?} refuses it \
                 ({refused:#}), so the node cannot attest the genesis it runs under; a fresh log is needed: \
                 {replace}",
                ProtocolVersion::LATEST
            );
        }
        if ran == genesis {
            return Ok(());
        }
        let index = ran.iter().zip(genesis).take_while(|(got, want)| got == want).count();
        bail!(
            "the log's genesis differs from the config's [genesis] at command {index}; start the node under \
             the [genesis] the log was started with"
        )
    }

    /// The genesis the log ran, as the config of a node that resumes the log gives it (see
    /// `restated`), with `weighting` the one the node carries into the log.
    #[cfg(test)]
    pub(crate) fn as_config(&self, weighting: Option<NotionalWeighting>) -> Vec<Command> {
        self.restated(weighting)
    }

    /// The genesis the log ran, in the form a config gives it under the latest protocol: parameters
    /// set before `V11` without a toxicity notional weighting take `weighting`, and absolute
    /// divergence triggers set before `V13`, which a config can no longer give, become shares of the
    /// widest divergence bound among the genesis markets.
    fn restated(&self, weighting: Option<NotionalWeighting>) -> Vec<Command> {
        let mut genesis = self.commands.clone();
        let widest = genesis
            .iter()
            .filter_map(|cmd| match cmd {
                Command::Admin(AdminCommand::UpsertMarket(spec)) => Some(spec.price_rules.max_divergence_bps),
                _ => None,
            })
            .max();
        if let Some(Command::Admin(AdminCommand::SetParams(params))) = genesis.first_mut() {
            params.toxicity.notional = params.toxicity.notional.or(weighting);
            params.share_divergence_triggers(widest.unwrap_or(0));
        }
        genesis
    }
}

/// The commands that sequence `genesis` into a log, in order. The last is the house capital credit
/// under `GENESIS_REF`, a zero one included, which marks genesis done.
pub fn genesis_commands(genesis: &Genesis) -> Vec<Command> {
    let mut cmds = vec![Command::Admin(AdminCommand::SetParams(genesis.params.clone()))];
    cmds.extend(genesis.benchmarks.iter().map(|b| {
        Command::Admin(AdminCommand::SetBenchmarkRate { benchmark: b.name.clone(), annual_bps: b.annual_bps })
    }));
    cmds.extend(genesis.markets.iter().map(|m| Command::Admin(AdminCommand::UpsertMarket(m.clone()))));
    if let Some(staking) = &genesis.staking {
        cmds.push(Command::Admin(AdminCommand::InitStaking(*staking)));
    }
    if !genesis.close_spread_ceilings.is_empty() {
        cmds.push(Command::Admin(AdminCommand::SetCloseSpreadCeilings {
            ceilings: genesis.close_spread_ceilings.clone(),
        }));
    }
    cmds.push(Command::HouseCapital { amount: genesis.house_capital, chain_ref: GENESIS_REF.into() });
    cmds
}

/// Whether `cmd` is one of the kinds `genesis_commands` gives.
fn is_genesis_command(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Admin(
            AdminCommand::SetParams(_)
                | AdminCommand::SetBenchmarkRate { .. }
                | AdminCommand::UpsertMarket(_)
                | AdminCommand::InitStaking(_)
                | AdminCommand::SetCloseSpreadCeilings { .. }
        )
    ) || is_genesis_credit(cmd)
}

/// Whether `cmd` is the house capital credit that ends genesis.
fn is_genesis_credit(cmd: &Command) -> bool {
    matches!(cmd, Command::HouseCapital { chain_ref, .. } if chain_ref == GENESIS_REF)
}

/// Applies protocol activation under `client_auth`, then `genesis`, to an empty engine, as a fresh
/// log sequences them. Genesis runs only on a log that holds no more than protocol activation, so
/// this engine accepts exactly the commands the log's engine would.
/// Error cases:
/// - the engine rejects a command; the error names the genesis entry it comes from
pub(crate) fn check_genesis(client_auth: &ClientAuth, operator: Operator, genesis: &Genesis) -> anyhow::Result<()> {
    check_genesis_commands(client_auth, operator, genesis_commands(genesis))
}

/// As `check_genesis`, for the commands a genesis sequences.
/// Error cases:
/// - the engine rejects a command; the error names the genesis entry it comes from
fn check_genesis_commands(client_auth: &ClientAuth, operator: Operator, genesis: Vec<Command>) -> anyhow::Result<()> {
    let mut engine = Engine::new();
    let cmds = activation_commands(&engine, client_auth, operator, Vec::new()).into_iter().chain(genesis);
    for (seq, cmd) in (0..).zip(cmds) {
        let sequenced = Sequenced { seq, ts: 0, cmd };
        let events = engine.apply(&sequenced).context("checking genesis")?;
        if let Some(reason) = rejection(&events) {
            bail!("{}: the engine rejects it: {reason}", config_entry(&sequenced.cmd));
        }
    }
    Ok(())
}

/// The reason the engine gave for rejecting an admin or house capital command, if it did.
pub(crate) fn rejection(events: &[Event]) -> Option<&str> {
    events.iter().find_map(|e| match e {
        Event::AdminRejected { reason } => Some(reason.as_str()),
        _ => None,
    })
}

/// The config entry a genesis command comes from, for an error that names it.
fn config_entry(cmd: &Command) -> String {
    match cmd {
        Command::Admin(AdminCommand::SetParams(_)) => "genesis.params".into(),
        Command::Admin(AdminCommand::SetBenchmarkRate { benchmark, .. }) => format!("genesis benchmark {benchmark}"),
        Command::Admin(AdminCommand::UpsertMarket(spec)) => format!("genesis market {}", spec.symbol),
        Command::Admin(AdminCommand::InitStaking(_)) => "genesis.staking".into(),
        Command::Admin(AdminCommand::SetCloseSpreadCeilings { .. }) => "genesis.close_spread_ceilings".into(),
        Command::HouseCapital { .. } => "genesis.house_capital".into(),
        _ => "protocol activation".into(),
    }
}
