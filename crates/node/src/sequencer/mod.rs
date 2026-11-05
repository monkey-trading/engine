//! The sequencer owns the engine. Commands arrive over one channel, get a seq and a timestamp, and
//! are applied in order; each then goes into the log with the events it produced. Nothing is
//! acknowledged or published until its log entry is written and synced to storage. One task, no
//! locks, and no queue beyond the channel: a fill costs about one engine step plus a buffered
//! write, and each batch of fills one data sync.

mod catch_up;
mod config;
pub use config::SequencerConfig;
pub(crate) mod history_rebuild;
pub(crate) mod legacy;
mod marks;
pub(crate) mod origin;
mod recovery;
mod replay;
pub(crate) use replay::replay_path_with_authorities;
pub use replay::{
    Replayed, ReplayedLog, replay, replay_path, replay_path_in_domain, replay_path_with_attested,
    replay_path_with_trust, replay_with_signer,
};
mod checkpoints;
use checkpoints::publish::After;
use checkpoints::{Gate, HandoverGate};
mod claims;
pub use claims::ClaimAnswer;
mod messages;
pub use messages::{
    AccountPayouts, HandoverRefused, PayoutView, Query, Reissue, ReissueScreening, Request, SignedWithdrawal,
    StreamBody, StreamMsg,
};
mod access;
mod benchmark_schedule;
pub(crate) mod house;
mod payout_schedule;
#[cfg(test)]
mod payout_tests;
mod payouts;
mod pipeline;
use pipeline::{Batch, Pipeline};
#[cfg(test)]
mod withdrawal_scenario;
use access::AccessAlerts;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use engine_core::Engine;
#[cfg(test)]
use engine_core::Fixed;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::{Command, Sequenced};
use engine_core::deposits::LegacyDeposit;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{info, info_span, warn};

use crate::benchmarks::BenchmarkSchedule;
use crate::checkpoints::accounting::Accounting;
use crate::clock::Clock;
use crate::log::{CommandEntry, LogEntry, LogWriter, Offset, SegmentedLog};
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
    /// The seq the last written checkpoint covers, wherever this process found or wrote it:
    /// `checkpoint` deduplicates against it, so a scheduled and an explicit checkpoint cannot
    /// meet at one seq.
    dedup_checkpoint_seq: Option<u64>,
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
    access_policy: Option<engine_core::access::AccessPolicy>,
    /// The close-spread ceilings the config's genesis adopts, which an upgrade to `V40` carries
    /// into a legacy log before its activation (see `Sequencer::activate_protocol`).
    close_spread_ceilings: Vec<engine_core::close_spread::CloseSpreadCeiling>,
    /// The alert for house NAV below zero.
    deficit: DeficitAlert,
    /// Benchmark clears already emitted during this process, limited to listed names.
    benchmark_alerts: BenchmarkAlerts,
    access_alerts: AccessAlerts,
    operator: Operator,
    /// The registry's operator in effect at startup, while rotation is off (see `hold_has_signer`).
    registry_operator: Option<crate::checkpoints::RegistryOperator>,
    /// This enclave's release, which takes the daily turns the log assigns `operator` (see `take_turn`).
    turn_release: Option<engine_core::eip712::B256>,
    metrics: Arc<Metrics>,
    gate: Option<Gate>,
    claims: claims::Claims,
    accounting: Accounting,
    /// Startup and recovery wait for a signed, complete publisher attempt before catch-up.
    benchmark_sync: bool,
    benchmark_sync_enabled: bool,
    /// A `benchmark_sync_skipped` episode may be open, until a snapshot applies: from each start, since
    /// the alert outbox keeps an episode an earlier start opened, and from each lifted hold.
    benchmark_sync_skipped: bool,
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
    /// Live mark reporting for the derived marks cache, and the dashboard record fed beside it.
    marks: marks::Marks,
    house: house::Latest,
    /// The thread that syncs the log, once `pipeline_log_syncs` has started one.
    pipeline: Option<Pipeline>,
    /// What checkpoints written in the batch so far wait for its sync to do.
    after_sync: Vec<After>,
    /// The on-chain log-head poster's publish end: the released-head watch, and the rollback
    /// freeze latch the poster sets the moment it detects a rollback — before its `Freeze`
    /// request dequeues — so nothing queued behind the detection is stamped or checkpointed.
    log_heads: Option<crate::log_heads::Publisher>,
}

impl Sequencer {
    /// Whether a detected log rollback froze sequencing: no stamp and no checkpoint is admitted,
    /// and the drain writes no final checkpoint. The poster's latch, not the request queue, is
    /// the source: it is set before the `Freeze` request travels.
    fn frozen(&self) -> bool {
        self.log_heads.as_ref().is_some_and(|publisher| publisher.freeze.frozen())
    }

    /// Sequences requests until every sender is gone or an admitted handover stop ends the loop,
    /// then writes a final checkpoint. Each batch, of at most `MAX_BATCH` requests, is written and
    /// its data synced before its replies, stream messages, and mark reports go out; one that
    /// carries a sync request or a signed withdrawal also syncs the directory entry naming its
    /// segment. With `pipeline_log_syncs` the sync runs on its own thread while the next requests
    /// are sequenced, and batches are released in order as their syncs finish. Queries run on the
    /// sequencer's thread right after the batch is written, so they can see commands whose batch is
    /// still syncing: a view a client sees reads the stats worker's copy instead (`ApiState::read`). A command trusted time
    /// refuses is dropped with its reply, which the API answers as unavailable, as is any command
    /// after a `Freeze`, a detected log rollback. A handover's final
    /// checkpoint, once admitted, is written before the stop's reply and the loop ends, so what the
    /// handover hands over is never behind the last sequenced command.
    /// Error cases:
    /// - the log cannot be written or synced; the node must stop rather than acknowledge
    ///   undurable commands
    /// - the engine rejects a queued required command
    pub async fn run(mut self, mut rx: mpsc::Receiver<Request>) -> anyhow::Result<()> {
        let mut batch = Batch::default();
        let mut queries: Vec<Query> = Vec::new();
        while let Some(first) = self.next_request(&mut rx).await? {
            let started = self.clock.instant();
            let mut sequenced = false;
            let (mut next, mut taken) = (Some(first), 0);
            let mut handover_stop = None;
            while let Some(req) = next.take() {
                taken += 1;
                match req {
                    Request::Submit { cmd, reply } => {
                        let events = self.sequence(cmd)?;
                        sequenced |= events.is_some();
                        if let (Some(events), Some(reply)) = (events, reply) {
                            batch.replies.push((reply, events));
                        }
                    }
                    Request::Quote { cmd, not_after } => sequenced |= self.sequence_quote(cmd, not_after)?.is_some(),
                    Request::Query(q) => queries.push(q),
                    Request::HouseState(reply) => self.house.wait(reply),
                    Request::TimeSample(sample) => self.take_sample(sample)?,
                    Request::Sync(reply) => batch.syncs.push(reply),
                    Request::WatchBenchmarkSchedule(reply) => batch.watches.push(reply),
                    Request::ReissueWithdrawal { account, nonce, replaces, screening, reply } => {
                        let answer = self.request_reissue(account, nonce, replaces, screening.as_ref());
                        batch.later.push(Box::new(move || _ = reply.send(answer)));
                    }
                    Request::AccountPayouts { account, after, limit, reply } => {
                        let answer = self.account_payouts(account, after, limit);
                        batch.later.push(Box::new(move || _ = reply.send(answer)));
                    }
                    Request::EntitlementClaim { account, reply } => {
                        let answer = self.claims.entitlement(&self.engine, account);
                        batch.later.push(Box::new(move || _ = reply.send(answer)));
                    }
                    Request::DistributionClaim { account, reply } => {
                        let answer = self.claims.distribution(&self.engine, account);
                        batch.later.push(Box::new(move || _ = reply.send(answer)));
                    }
                    Request::CheckpointStatus { posted, latest, health, lifecycle } => {
                        if let Some(latest) = latest {
                            self.claims.accept(latest);
                        }
                        self.custody_status(posted, health, lifecycle)?;
                    }
                    Request::FinalCheckpoint(reply) => {
                        // Stopping for a handover is the checkpoint gate's call: refused, the node
                        // goes on sequencing; admitted, nothing after this request sequences and the
                        // loop ends once the final checkpoint is durable.
                        match self.handover_checkpoint()? {
                            HandoverGate::Admitted => handover_stop = Some(reply),
                            HandoverGate::Refused(reason) => {
                                let _ = reply.send(Err(reason));
                            }
                        }
                    }
                    // The poster latched the freeze before sending this; nothing queued behind the
                    // detection is stamped, and the drain writes no final checkpoint.
                    Request::Freeze => {}
                }
                if taken < MAX_BATCH && handover_stop.is_none() {
                    next = self.next_queued(&mut rx);
                }
            }
            batch.timing = sequenced.then_some(started);
            let release = self.stage(std::mem::take(&mut batch))?;
            self.submit(release)?;
            queries.drain(..).for_each(|q| q(&self.engine));
            if handover_stop.is_some() {
                self.settle_async().await?;
            }
            // `seal_handover_tail` answers a failed seal as the stop's refusal: sequencing goes on.
            if self.seal_handover_tail(handover_stop) {
                break;
            }
        }
        self.settle_async().await?;
        // A frozen sequencer — a detected log rollback — writes no final checkpoint: what it would
        // attest is exactly what must not stand as healthy history.
        if !self.frozen() && (self.last_checkpoint != self.engine.last_seq() || self.uncaptured.is_some()) {
            let final_checkpoint = self.last_checkpoint != self.engine.last_seq();
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
    /// - as `sequence_stamped`
    fn sequence(&mut self, cmd: Command) -> anyhow::Result<Option<Vec<Event>>> {
        self.sequence_stamped(cmd, None)
    }

    /// `sequence`, with the stamp also refused for a venue's quote that `quote` names (see
    /// `admitted_ts`).
    /// Error cases:
    /// - the log cannot be written
    /// - the engine rejects a queued required command
    /// - the engine refuses the sequence, which means this sequencer is broken
    fn sequence_stamped(&mut self, mut cmd: Command, quote: Option<Instant>) -> anyhow::Result<Option<Vec<Event>>> {
        let Some(ts) = self.admitted_ts(quote) else {
            return Ok(None);
        };
        self.sequence_queued(ts)?;
        if !access::gate_withdrawal(&mut cmd, self.gate.as_ref().map(checkpoints::Gate::admits_withdrawals)) {
            return Ok(None);
        }
        // A turn's first command is its `TurnTaken`, even behind a benchmark hold: it runs no catch-up.
        self.take_turn(ts)?;
        if self.benchmark_sync && !matches!(&cmd, Command::SignedAdmin(req) if req.command.is_benchmark_snapshot()) {
            return Ok(None);
        }
        let snapshot = matches!(&cmd, Command::SignedAdmin(req) if req.command.is_benchmark_snapshot());
        let Some(events) = self.stamp(ts, cmd)? else {
            // Frozen mid-request: refuse like an unadmitted stamp, dropping the reply.
            return Ok(None);
        };
        if snapshot && events.contains(&Event::AdminApplied) {
            self.benchmark_schedule_pending = false;
            if std::mem::take(&mut self.benchmark_sync_skipped) {
                info!(alert_resolved = "benchmark_sync_skipped", "publisher snapshot installed");
            }
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
            let (host_ms, now) = (self.clock.now_ms(), self.clock.instant());
            let ts = self.time.stamp(host_ms, self.last_ts, now);
            self.benchmark_time_refused |= !self.time.can_admit(host_ms, ts, now);
        }
        self.time.update(sample);
        if self.benchmark_sync_enabled {
            if let Some(ts) = self.admitted_ts(None) {
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

    /// Applies one command at `ts`, logs it with its events, signs any approved withdrawal, and
    /// checkpoints on schedule. The command's span carries its seq as `id`.
    /// Error cases:
    /// - the log cannot be written
    /// - the engine refuses the sequence, which means this sequencer is broken
    fn stamp_once(&mut self, ts: Millis, cmd: Command) -> anyhow::Result<Vec<Event>> {
        if self.frozen() {
            // The common per-command boundary: a rollback that froze sequencing while this request
            // was admitted — mid catch-up, or before its own stamp — refuses here, like an
            // unadmitted stamp, with nothing logged.
            return Ok(Vec::new());
        }
        let previous_schedule =
            house::set_params_of(&cmd).map(|_| self.engine.params().map(|params| params.rollover.clone()));
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
        house::note_params(&mut self.progress.params_seq, &sequenced, &events);
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
        self.access_alerts.note(&self.engine, &sequenced.cmd, &events);
        self.report_marks(&sequenced, &events);
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
        self.metrics.access_pending_deposits.set(self.engine.pending_deposits().len() as i64);
        for (event_index, event) in events.iter().enumerate() {
            alert_on(event);
            self.benchmark_alerts.note(event);
            self.metrics.record(event);
            if let Event::WithdrawalApproved { account, amount, nonce } = event {
                if let Some(approval) = self.engine.withdrawal_approval(*account, *nonce).copied() {
                    if let Some(gate) = &mut self.gate {
                        gate.payouts.insert((*account, *nonce), payouts::Payout::new(approval, event_index));
                        gate.schedule.insert((approval.approved_seq, event_index), (*account, *nonce), true);
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
            self.paced_checkpoint()?;
        }
        Ok(events)
    }

    /// Appends an entry to the log and queues it for the stream at its offset.
    /// Error cases:
    /// - the log cannot be written
    fn record(&mut self, entry: LogEntry) -> anyhow::Result<Offset> {
        let offset = self.log.append(&entry)?;
        self.outbox.push(Arc::new(StreamMsg::new(offset, entry)));
        Ok(offset)
    }
}

mod alerts;
pub(crate) mod genesis;
mod snapshots;
pub use genesis::{GenesisProgress, genesis_commands};
mod quotes;
mod required;
mod turns;

#[cfg(test)]
mod batch_tests;
#[cfg(test)]
mod margin_tests;
#[cfg(test)]
mod metrics_tests;
#[cfg(test)]
mod pace_tests;
#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
mod wait_bench;

#[cfg(test)]
mod checkpoint_tests;
#[cfg(test)]
pub(crate) mod fixture_tests;
