//! The sequencer owns the engine. Commands arrive over one channel, get a seq and a timestamp, and
//! are applied in order; each then goes into the log with the events it produced. Nothing is
//! acknowledged or published until its log entry is written and synced to storage. One task, no
//! locks, and no queue beyond the channel: a fill costs about one engine step plus a buffered
//! write, and each batch of fills one data sync.

pub(crate) mod history_rebuild;
pub(crate) mod legacy;
mod origin;
mod recovery;
mod replay;
pub(crate) use replay::replay_path_with_authorities;
pub use replay::{Replayed, replay, replay_path, replay_path_in_domain, replay_path_with_trust, replay_with_signer};
mod checkpoints;
use checkpoints::Gate;
mod messages;
pub use messages::{Query, Request, SignedWithdrawal, StreamBody, StreamMsg};
mod benchmark_schedule;
mod payout_schedule;
#[cfg(test)]
mod payout_tests;
mod payouts;
#[cfg(test)]
mod withdrawal_scenario;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::deposits::LegacyDeposit;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};
use engine_core::{Engine, Fixed};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{error, info, info_span, warn};

use crate::benchmarks::BenchmarkSchedule;
use crate::checkpoints::accounting::Accounting;
use crate::clock::Clock;
use crate::log::{CommandEntry, LogEntry, LogWriter, Manifest, SegmentedLog};
use crate::metrics::Metrics;
use crate::sequencer::alerts::{BenchmarkAlerts, DeficitAlert, alert_on, reinstated_receipt, resolve_reversal};
use crate::snapshot::{Progress, RecoveryBase};
use crate::tee::EnclaveKey;
use crate::trusted_time::{Sample, TimeGate};

/// Most requests of any kind taken into one batch before it is flushed and answered.
const MAX_BATCH: usize = 1024;

/// How long the drain waits for an in-flight snapshot capture before its final checkpoint, well
/// inside the node's ten-second shutdown grace.
const SHUTDOWN_CAPTURE_WAIT: Duration = Duration::from_secs(5);

pub struct Sequencer {
    legacy_deposits: BTreeMap<String, LegacyDeposit>,
    legacy_approvals: legacy::Approvals,
    legacy_authorizations: legacy::Authorizations,
    engine: Engine,
    log: LogWriter,
    key: Arc<EnclaveKey>,
    command_link: engine_core::eip712::B256,
    checkpoint_every: u64,
    next_seq: u64,
    last_ts: Millis,
    last_checkpoint: Option<u64>,
    chain_id: u64,
    vault: AccountId,
    stream: broadcast::Sender<Arc<StreamMsg>>,
    withdrawals: mpsc::UnboundedSender<SignedWithdrawal>,
    outbox: Vec<Arc<StreamMsg>>,
    clock: Arc<dyn Clock>,
    time: TimeGate,
    /// Commands the engine must accept, waiting for a stamp trusted time admits, each group with
    /// what it is for.
    required: Vec<(&'static str, Vec<Command>)>,
    client_auth: ClientAuth,
    /// The alert for house NAV below zero.
    deficit: DeficitAlert,
    /// Benchmark clears already emitted during this process, limited to listed names.
    benchmark_alerts: BenchmarkAlerts,
    operator: Operator,
    metrics: Arc<Metrics>,
    gate: Option<Gate>,
    accounting: Accounting,
    /// Startup and recovery wait for a signed, complete publisher attempt before catch-up.
    benchmark_sync: bool,
    benchmark_sync_enabled: bool,
    benchmark_time_refused: bool,
    benchmark_schedule: watch::Sender<BenchmarkSchedule>,
    benchmark_schedule_dirty: bool,
    benchmark_schedule_pending: bool,
    sealing: Option<crate::snapshot::sealing::Endpoint>,
    progress: Progress,
    command_prefix: [u8; 32],
    snapshot_base: Option<RecoveryBase>,
    snapshot_work: Vec<crate::checkpoints::Work>,
    snapshot_anchor_work: Option<crate::checkpoints::Work>,
    snapshot_signers: BTreeSet<AccountId>,
    /// The newest checkpoint, with its recovery facts as they stood then, when its capture was
    /// skipped because another was in flight.
    uncaptured: Option<snapshots::Deferred>,
}

pub struct SequencerConfig<'a> {
    pub log_dir: &'a Path,
    pub segment_commands: u64,
    /// Where the log writer publishes its manifest.
    pub manifest: watch::Sender<Manifest>,
    pub checkpoint_every: u64,
    pub chain_id: u64,
    pub vault: AccountId,
    /// The clocks commands are stamped and checked with.
    pub clock: Arc<dyn Clock>,
    /// The most a stamp may differ from trusted time.
    pub max_drift: Duration,
    /// The client auth the config gives, which a log that records one must match.
    pub client_auth: ClientAuth,
    /// The operator the config gives, which a log that records one must match.
    pub operator: Operator,
    /// Where the sequencer records its metrics.
    pub metrics: Arc<Metrics>,
    pub(crate) checkpoint_registry: Option<crate::checkpoints::Registry>,
    pub(crate) approval_registry: Option<crate::checkpoints::ApprovalRegistry>,
    pub(crate) checkpoint_parent: crate::log::CheckpointParent,
    pub(crate) checkpoints: Option<mpsc::Sender<crate::checkpoints::Work>>,
    /// The commands the config's genesis sequences, which the genesis the log ran must match (see
    /// `GenesisProgress::ensure_matches`).
    pub genesis: Vec<Command>,
    pub(crate) history_sealing: Option<crate::snapshot::sealing::Endpoint>,
}

/// A log read from disk and replayed, before anything is written to it.
pub struct ReplayedLog {
    log: SegmentedLog,
    replayed: Replayed,
    snapshot_seq: Option<u64>,
    legacy_authorizations: legacy::Authorizations,
}

impl Sequencer {
    /// Sequences requests until every sender is gone, then writes a final checkpoint. Each batch,
    /// of at most `MAX_BATCH` requests, is written and its data synced before its replies, stream
    /// messages, and queries go out; one that carries a sync request also syncs the directory entry
    /// naming its segment. A command trusted time refuses is dropped with its reply, which the API
    /// answers as unavailable.
    /// Error cases:
    /// - the log cannot be written or synced; the node must stop rather than acknowledge
    ///   undurable commands
    /// - the engine rejects a queued required command
    pub async fn run(mut self, mut rx: mpsc::Receiver<Request>) -> anyhow::Result<()> {
        let mut replies = Vec::new();
        let mut queries: Vec<Query> = Vec::new();
        let mut syncs = Vec::new();
        let mut benchmark_watches = Vec::new();
        while let Some(first) = rx.recv().await {
            let started = self.clock.instant();
            let mut sequenced = false;
            let (mut next, mut taken) = (Some(first), 0);
            while let Some(req) = next.take() {
                taken += 1;
                match req {
                    Request::Submit { cmd, reply } => {
                        let events = self.sequence(cmd)?;
                        sequenced |= events.is_some();
                        if let (Some(events), Some(reply)) = (events, reply) {
                            replies.push((reply, events));
                        }
                    }
                    Request::Query(q) => queries.push(q),
                    Request::TimeSample(sample) => self.take_sample(sample)?,
                    Request::Sync(reply) => syncs.push(reply),
                    Request::WatchBenchmarkSchedule(reply) => benchmark_watches.push(reply),
                    Request::CheckpointStatus { posted, health, lifecycle } => {
                        self.checkpoint_status(posted, health)?;
                        if let Some(evidence) = lifecycle {
                            self.release_payouts(&evidence)?;
                        }
                    }
                }
                if taken < MAX_BATCH {
                    next = rx.try_recv().ok();
                }
            }
            self.flush()?;
            if !syncs.is_empty() {
                self.log.sync()?;
                let last_seq = self.engine.last_seq();
                syncs.drain(..).for_each(|reply| {
                    let _ = reply.send(last_seq);
                });
            }
            replies.drain(..).for_each(|(reply, events)| {
                let _ = reply.send(events);
            });
            benchmark_watches.drain(..).for_each(|reply| {
                let _ = reply.send(self.benchmark_schedule.subscribe());
            });
            // Only admitted commands are written: a batch of reads or refusals would dilute the latency.
            if sequenced {
                self.metrics.batch_seconds.observe(self.clock.instant().duration_since(started).as_secs_f64());
            }
            queries.drain(..).for_each(|q| q(&self.engine));
        }
        let final_checkpoint = self.last_checkpoint != self.engine.last_seq();
        if final_checkpoint || self.uncaptured.is_some() {
            if let Some(endpoint) = self.sealing.clone()
                && tokio::time::timeout(SHUTDOWN_CAPTURE_WAIT, endpoint.idle()).await.is_err()
            {
                endpoint.write_alert.failed(|| {
                    warn!(
                        alert = "snapshot_write_failed",
                        "the shutdown checkpoint is not cached: a capture is still in flight"
                    )
                });
            }
            if final_checkpoint {
                self.checkpoint()?;
            } else if let Some(deferred) = self.uncaptured.take() {
                // The last command's checkpoint found a capture in flight; history accepts a
                // capture of the checkpoint it has just indexed. Its seal is collected like any
                // checkpoint's.
                self.capture(deferred)?;
                self.maintain_catalog();
            }
        }
        self.flush()?;
        info!(seq = ?self.engine.last_seq(), "sequencer drained");
        Ok(())
    }

    /// Stamps and sequences one command, after any queued required commands. Returns `None`, with
    /// nothing logged, when trusted time refuses the stamp.
    /// Error cases:
    /// - the log cannot be written
    /// - the engine rejects a queued required command
    /// - the engine refuses the sequence, which means this sequencer is broken
    fn sequence(&mut self, cmd: Command) -> anyhow::Result<Option<Vec<Event>>> {
        let Some(ts) = self.admitted_ts() else {
            return Ok(None);
        };
        self.sequence_queued(ts)?;
        if matches!(cmd, Command::Withdraw { .. })
            && self.gate.as_ref().is_some_and(|gate| !matches!(gate.health, Some(crate::checkpoints::Health::Healthy)))
        {
            warn!(alert = "withdrawals_paused", "withdrawal refused until custody reconciles");
            return Ok(None);
        }
        if self.benchmark_sync && !matches!(&cmd, Command::SignedAdmin(req) if req.command.is_benchmark_snapshot()) {
            return Ok(None);
        }
        let snapshot = matches!(&cmd, Command::SignedAdmin(req) if req.command.is_benchmark_snapshot());
        let events = self.stamp(ts, cmd)?;
        if snapshot && events.contains(&Event::AdminApplied) {
            self.benchmark_schedule_pending = false;
        }
        if self.benchmark_sync && snapshot && events.contains(&Event::AdminApplied) {
            self.benchmark_sync = false;
            info!(alert_resolved = "benchmark_sync_pending", "publisher snapshot installed; sequencing resumed");
            self.sequence_queued(ts)?;
        }
        Ok(Some(events))
    }

    /// Takes `sample` as the latest trusted time, and sequences any queued required commands it
    /// admits.
    /// Error cases:
    /// - the engine rejects a queued required command
    /// - the log cannot be written
    fn take_sample(&mut self, sample: Sample) -> anyhow::Result<()> {
        if self.benchmark_sync_enabled {
            let host_ms = self.clock.now_ms();
            self.benchmark_time_refused |=
                !self.time.can_admit(host_ms, host_ms.max(self.last_ts), self.clock.instant());
        }
        self.time.update(sample);
        if self.benchmark_sync_enabled {
            if let Some(ts) = self.admitted_ts() {
                self.sequence_queued(ts)?;
            }
            Ok(())
        } else {
            self.sequence_queued_if_admitted()
        }
    }

    /// `take_sample` outside `run`, flushing what it sequences: how a test gives a sequencer trusted
    /// time before it runs.
    /// Error cases:
    /// - as `take_sample`, or the log cannot be flushed
    #[cfg(test)]
    pub fn trust(&mut self, sample: Sample) -> anyhow::Result<()> {
        self.take_sample(sample)?;
        self.flush()
    }

    /// The state root of the engine a test's recovered sequencer holds.
    #[cfg(test)]
    pub fn state_root(&mut self) -> [u8; 32] {
        self.engine.state_root()
    }

    /// The stamp for a command now, if trusted time admits it. The host clock is untrusted inside
    /// a TEE: it can't move sequenced time backwards, and nothing is logged unless the clock and the
    /// stamp are within the drift bound of trusted time.
    fn admitted_ts(&mut self) -> Option<Millis> {
        let host_ms = self.clock.now_ms();
        let ts = host_ms.max(self.last_ts);
        let admitted = self.time.admit(host_ms, ts, self.clock.instant());
        self.note_benchmark_time_admission(admitted, ts);
        if admitted {
            self.admit_benchmark_schedule(ts);
        }
        admitted.then_some(ts)
    }

    /// Applies one command at `ts`, logs it with its events, signs any approved withdrawal, and
    /// checkpoints on schedule. The command's span carries its seq as `id`.
    /// Error cases:
    /// - the log cannot be written
    /// - the engine refuses the sequence, which means this sequencer is broken
    fn stamp(&mut self, ts: Millis, cmd: Command) -> anyhow::Result<Vec<Event>> {
        let parameters = match &cmd {
            Command::Admin(command) => Some(command),
            Command::SignedAdmin(request) => Some(&request.command),
            _ => None,
        };
        let previous_schedule = parameters
            .filter(|command| matches!(command, AdminCommand::SetParams(_)))
            .map(|_| self.engine.params().map(|params| params.rollover.clone()));
        let previous_benchmarks =
            matches!(&cmd, Command::Admin(_) | Command::SignedAdmin(_)).then(|| BenchmarkAlerts::listed(&self.engine));
        let sequenced = Sequenced { seq: self.next_seq, ts, cmd };
        let _span = info_span!("command", id = sequenced.seq).entered();
        legacy::check_activation_ledger(&sequenced.cmd, &self.legacy_deposits)?;
        let version = self.engine.protocol_version();
        let events = self.engine.apply(&sequenced).context("sequencer produced an invalid sequence")?;
        #[cfg(test)]
        withdrawal_scenario::before_approval_log(&sequenced, &events)?;
        self.accounting.apply(&sequenced.cmd, &events)?;
        self.progress.genesis.command(&sequenced, &events);
        if self.engine.protocol_version() != version {
            self.progress.activations.push((self.engine.protocol_version(), sequenced.seq));
        }
        if !self.engine.deposit_finality_enabled()
            && let Some(deposit) = LegacyDeposit::from_command(&sequenced.cmd, &events)
        {
            self.legacy_deposits.insert(deposit.chain_ref.clone(), deposit);
        }
        self.legacy_approvals.observe(&self.engine, &sequenced, &events, true);
        let reinstated = reinstated_receipt(&sequenced.cmd, &events);
        self.next_seq += 1;
        self.last_ts = ts;
        let mut entry = CommandEntry { sequenced, events: Some(events.clone()), seal: None };
        let link = crate::log::command_seal::command_link(self.command_link, &entry)?;
        let domain = crate::log::CheckpointDomain { chain_id: self.chain_id, vault: self.vault };
        entry.seal = Some(self.key.seal_command(domain, link));
        self.command_prefix =
            crate::log::extend_command_prefix(self.command_prefix, crate::log::command_digest(&entry)?);
        self.record(LogEntry::Command(entry))?;
        self.command_link = link;
        if let Some(previous) = previous_schedule.filter(|_| events.contains(&Event::AdminApplied)) {
            self.note_benchmark_schedule(previous);
        }
        self.metrics.commands.inc();

        for (event_index, event) in events.iter().enumerate() {
            alert_on(event);
            self.benchmark_alerts.note(event);
            self.metrics.record(event);
            if let Event::WithdrawalApproved { account, amount, nonce } = event {
                if let Some(approval) = self.engine.withdrawal_approval(*account, *nonce).copied() {
                    if let Some(gate) = &mut self.gate {
                        gate.payouts.insert((*account, *nonce), payouts::Payout::new(approval, event_index));
                        gate.schedule.insert((approval.approved_seq, event_index), (*account, *nonce));
                        gate.unproved.insert((*account, *nonce));
                        gate.unregistered.insert((*account, *nonce));
                    } else {
                        self.sign_mock_payout(approval)?;
                    }
                } else if let Some(gate) = &mut self.gate {
                    gate.pending.push((
                        self.engine.last_seq().expect("applied command has a seq"),
                        *account,
                        *amount,
                        *nonce,
                    ));
                } else {
                    self.sign_withdrawal(*account, *amount, *nonce)?;
                }
            }
        }
        if let Some(chain_ref) = reinstated {
            resolve_reversal(&chain_ref);
        }
        if let Some(previous) = previous_benchmarks.filter(|_| events.contains(&Event::AdminApplied)) {
            self.benchmark_alerts.retain_listed(&self.engine, &previous);
        }
        self.deficit.note(self.engine.house_nav());
        if self.last_checkpoint != self.engine.last_seq()
            && (self.next_seq.is_multiple_of(self.checkpoint_every)
                || (self.gate.is_some()
                    && events.iter().any(|event| matches!(event, Event::WithdrawalApproved { .. }))))
        {
            self.checkpoint()?;
        }
        Ok(events)
    }

    /// Appends an entry to the log and queues it for the stream at its offset.
    /// Error cases:
    /// - the log cannot be written
    fn record(&mut self, entry: LogEntry) -> anyhow::Result<()> {
        let offset = self.log.append(&entry)?;
        self.outbox.push(Arc::new(StreamMsg::new(offset, entry)));
        Ok(())
    }

    /// Signs an approved withdrawal and logs it; custody and the stream get it once it is flushed.
    /// Error cases:
    /// - the log cannot be written
    fn sign_withdrawal(&mut self, account: AccountId, amount: Fixed, nonce: u64) -> anyhow::Result<()> {
        let seq = self.engine.last_seq().context("withdrawal without approval sequence")?;
        if self.withdrawal_release_seq(account, nonce, seq, None)?.is_none() {
            error!(alert = "legacy_withdrawal_unproven", %account, nonce, "inherited withdrawal lacks canonical finality proof");
            bail!("inherited withdrawal lacks canonical finality proof");
        }
        self.log.sync()?;
        let signature = self.key.sign_withdrawal(self.chain_id, &self.vault.0, account, amount, nonce);
        info!(%account, nonce, "withdrawal signed");
        self.record(LogEntry::Withdrawal(SignedWithdrawal {
            account,
            amount,
            nonce,
            chain_id: self.chain_id,
            vault: self.vault,
            signature,
            approved_seq: None,
            authorization: None,
        }))?;
        self.legacy_approvals.signed(account, amount, nonce);
        self.legacy_authorizations.remove(&(account, nonce));
        Ok(())
    }

    /// Makes buffered log entries durable, then publishes what they produced and hands signed
    /// withdrawals to custody.
    /// Error cases:
    /// - the log cannot be written
    fn flush(&mut self) -> anyhow::Result<()> {
        if self.outbox.iter().any(|msg| matches!(msg.body, StreamBody::Withdrawal(_))) {
            self.log.sync()?;
        } else {
            self.log.flush()?;
        }
        if std::mem::take(&mut self.benchmark_schedule_dirty) {
            self.benchmark_schedule.send_replace(BenchmarkSchedule::from_engine(&self.engine));
        }
        for msg in self.outbox.drain(..) {
            if let StreamBody::Withdrawal(w) = &msg.body
                && self.withdrawals.send(w.clone()).is_err()
            {
                let (seq, account, nonce) = (msg.offset.seq, w.account, w.nonce);
                error!(alert = "custody_unavailable", seq, %account, nonce, "signed withdrawal has no custody consumer");
            }
            let _ = self.stream.send(msg);
        }
        Ok(())
    }
}

mod alerts;
pub(crate) mod genesis;
mod snapshots;
pub use genesis::{GenesisProgress, genesis_commands};
mod required;

#[cfg(test)]
mod batch_tests;
#[cfg(test)]
mod margin_tests;
#[cfg(test)]
mod metrics_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod checkpoint_tests;
#[cfg(test)]
pub(crate) mod fixture_tests;
