//! Genesis: the `[genesis]` config as the commands that open a log, the check that the engine
//! accepts every one of them before any is written, and how far a replayed log got through them.
//! Genesis is all or nothing: recovery refuses a log that started genesis without reaching the
//! checkpoint after its house capital credit, and one whose genesis is not the config's.

use anyhow::{Context, bail};
use engine_core::Engine;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::params::NotionalWeighting;

use crate::config::Genesis;
use crate::sequencer::required::activation_commands;

/// The custody reference genesis credits house capital under, as its last command.
pub(super) const GENESIS_REF: &str = "genesis";
/// What an operator does about a log whose genesis did not apply in full, when nothing follows it.
const FRESH_LOG: &str = "with the node stopped, remove node.log_dir and history.dir to start a fresh log";
/// What an operator does when commands follow such a genesis: they may hold client funds.
const KEEP_LOG: &str = "commands follow genesis in the log, so keep it for an operator to examine";

/// How far a replayed log got through genesis. Genesis starts at the first command `genesis_commands`
/// gives, ends with its house capital credit, and closes at the next checkpoint. It applies in full
/// when it closes as one run the engine accepted throughout.
#[derive(Debug, Default)]
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
    /// the config hash the node attests names the genesis its engine runs under. Parameters genesis
    /// set before `V11` have no toxicity notional weighting: the config's stands in for it, and once
    /// the node has carried a weighting into the log, it must be that one.
    /// Error cases:
    /// - genesis started, and its commands differ from `genesis`
    pub fn ensure_matches(&self, genesis: &[Command]) -> anyhow::Result<()> {
        if self.started.is_none() {
            return Ok(());
        }
        let mut ran = self.commands.clone();
        if let (
            Some(Command::Admin(AdminCommand::SetParams(logged))),
            Some(Command::Admin(AdminCommand::SetParams(config))),
        ) = (ran.first_mut(), genesis.first())
            && logged.toxicity.notional.is_none()
        {
            logged.toxicity.notional = self.carried.or(config.toxicity.notional);
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

    /// The genesis the log ran, as the config of a node that resumes the log gives it: parameters set
    /// before `V11` take `weighting`, which the node carries into the log.
    #[cfg(test)]
    pub(crate) fn as_config(&self, weighting: Option<NotionalWeighting>) -> Vec<Command> {
        let mut genesis = self.commands.clone();
        if let Some(Command::Admin(AdminCommand::SetParams(params))) = genesis.first_mut() {
            params.toxicity.notional = params.toxicity.notional.or(weighting);
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
    cmds.push(Command::HouseCapital { amount: genesis.house_capital, chain_ref: GENESIS_REF.into() });
    cmds
}

/// Whether `cmd` is one of the kinds `genesis_commands` gives.
fn is_genesis_command(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Admin(
            AdminCommand::SetParams(_) | AdminCommand::SetBenchmarkRate { .. } | AdminCommand::UpsertMarket(_)
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
    let mut engine = Engine::new();
    let cmds = activation_commands(&engine, client_auth, operator).into_iter().chain(genesis_commands(genesis));
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
pub(super) fn rejection(events: &[Event]) -> Option<&str> {
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
        Command::HouseCapital { .. } => "genesis.house_capital".into(),
        _ => "protocol activation".into(),
    }
}
