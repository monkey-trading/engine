//! The operator's side of the boundary's log check: once its sequencer publishes a check due, its node
//! asks the members of the turn's order, first position first, for an attested verification of the
//! checkpoint, signing each question with its enclave key, skipping its own candidate, each member that
//! sequenced the covered range under any release, each release that did, and each member with no
//! handover endpoint, and gives the first it admits to the sequencer. A round that no member answers is
//! tried again; a check that stays due pages `log_check_pending`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use ring::rand::{SecureRandom, SystemRandom};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, timeout};
use tracing::{Instrument, error, info, info_span, warn};

use crate::checkpoints::{CheckpointChain, HandoverSource, View};
use crate::config::TeeProvider;
use crate::handover::evidence;
use crate::handover::{MESSAGE_TIMEOUT, no_weaker, receive, send};
use crate::log::CheckpointDomain;
use crate::log_check::{Answer, Ask, Due, Sequencers, Statement, ask_digest, report_data};
use crate::sequencer::Request;
use crate::tee::EnclaveKey;
use crate::tee::sealing::{Platform, Release};
use crate::trusted_time::TrustedClock;

/// How long one member gets to answer, its quote's collateral fetch included.
const ASK_TIMEOUT: Duration = if cfg!(test) { Duration::from_secs(5) } else { Duration::from_secs(90) };
/// How long after a round no member answered the node asks again.
const RETRY: Duration = if cfg!(test) { Duration::from_millis(200) } else { Duration::from_secs(60) };
/// How long a check may stay due before it pages: withdrawals approved since the last verified head
/// wait for it.
const PENDING_ALERT_AFTER: Duration =
    if cfg!(test) { Duration::from_millis(600) } else { Duration::from_secs(30 * 60) };

/// The operator node's asking for the checks its sequencer publishes as due.
pub(crate) struct Asker<C> {
    vault: C,
    sequencer: mpsc::WeakSender<Request>,
    due: watch::Receiver<Option<Due>>,
    domain: CheckpointDomain,
    provider: TeeProvider,
    /// This enclave's platform, which no verifier's policy or TCB may be weaker than.
    platform: Platform,
    /// The enclave key that signs this node's checkpoints, and so its questions.
    key: Arc<EnclaveKey>,
    trusted: TrustedClock,
}

impl<C: CheckpointChain> Asker<C> {
    /// Asks for each check `due` publishes, through `vault`'s finalized schedule, signing each question
    /// with `key`, and gives what it admits to `sequencer`, verifying quotes as `provider` does at
    /// `trusted` time.
    pub(crate) fn new(
        vault: C,
        sequencer: &mpsc::Sender<Request>,
        due: watch::Receiver<Option<Due>>,
        (domain, provider, platform): (CheckpointDomain, TeeProvider, Platform),
        key: Arc<EnclaveKey>,
        trusted: TrustedClock,
    ) -> Self {
        Self { vault, sequencer: sequencer.downgrade(), due, domain, provider, platform, key, trusted }
    }

    /// Asks for each check due until the sequencer stops.
    /// Error cases:
    /// - none: a round no member answers is tried again
    pub(crate) async fn run(mut self) -> anyhow::Result<()> {
        let mut pending: Option<(u64, Instant, bool)> = None;
        loop {
            let due = self.due.borrow_and_update().clone();
            let Some(due) = due else {
                if let Some((turn, _, true)) = pending.take() {
                    info!(alert_resolved = "log_check_pending", turn, "the boundary's log check verified the log");
                }
                if self.due.changed().await.is_err() {
                    return Ok(());
                }
                continue;
            };
            if pending.is_none_or(|(turn, ..)| turn != due.turn) {
                if let Some((turn, _, true)) = pending {
                    info!(alert_resolved = "log_check_pending", turn, "another turn's check is due now");
                }
                pending = Some((due.turn, Instant::now(), false));
            }
            let span = info_span!("log_check", turn = due.turn, seq = due.head.seq);
            match self.check(&due).instrument(span.clone()).await {
                Ok(true) => continue,
                Ok(false) => span.in_scope(|| info!("the sequencer no longer waits for this check")),
                Err(e) => {
                    let (turn, since, alerted) = pending.as_mut().expect("set above");
                    if !*alerted && since.elapsed() >= PENDING_ALERT_AFTER {
                        *alerted = true;
                        span.in_scope(|| {
                            warn!(
                                alert = "log_check_pending",
                                turn = *turn,
                                "no member has verified the log at the boundary: withdrawals approved since the last \
                                 verified head wait: {e:#}"
                            )
                        });
                    } else {
                        span.in_scope(|| warn!("no member verified the log this round: {e:#}"));
                    }
                }
            }
            tokio::select! {
                changed = self.due.changed() => if changed.is_err() { return Ok(()) },
                () = tokio::time::sleep(RETRY) => {}
            }
        }
    }

    /// Asks the qualifying members of `due`'s turn order, first to last, and gives the first
    /// verification it admits to the sequencer: whether the sequencer took it.
    /// Error cases:
    /// - the vault or the schedule cannot be read, or no member's verification is admitted
    async fn check(&self, due: &Due) -> anyhow::Result<bool> {
        let at = self.vault.observe(View::Finalized, &[]).await?;
        let signer_epoch = at.lifecycle.as_ref().map_or(1, |lifecycle| lifecycle.signer_epoch);
        let boundary = engine_core::turns::boundary(due.turn).context("the turn has no boundary")?;
        let statement =
            Statement { signer_epoch, boundary, turn: due.turn, position: due.position, set: due.set, head: due.head };
        let order = self.vault.turn_order(&at, due.turn).await.context("reading the turn's order")?;
        let mut failures = Vec::new();
        for member in qualifying(&order, due) {
            let asked = timeout(ASK_TIMEOUT, self.ask(member, &statement, &due.excluded)).await;
            match asked.unwrap_or_else(|_| Err(anyhow!("no answer in {ASK_TIMEOUT:?}"))) {
                Ok(release) => {
                    info!(verifier = %member.candidate, release = %release, "a member verified the log at the boundary");
                    return self.give(due, member.candidate, release).await;
                }
                Err(e) => {
                    warn!(member = %member.candidate, endpoint = %member.handover_endpoint, "this member's check was not admitted: {e:#}");
                    failures.push(format!("{}: {e:#}", member.candidate));
                }
            }
        }
        bail!(
            "no qualifying member of turn {}'s order verified seq {}: [{}]",
            due.turn,
            due.head.seq,
            failures.join("; ")
        )
    }

    /// Asks `member` to verify `statement`: the release it proves, once admitted (see `admit`).
    /// Error cases:
    /// - the connection or the exchange fails, or the answer is not admitted
    async fn ask(&self, member: &HandoverSource, statement: &Statement, excluded: &Sequencers) -> anyhow::Result<B256> {
        let mut nonce = [0u8; 32];
        SystemRandom::new().fill(&mut nonce).map_err(|_| anyhow!("the RNG failed"))?;
        let signature = self.key.sign_digest(ask_digest(self.domain, &nonce, statement).0);
        let mut stream = timeout(MESSAGE_TIMEOUT, TcpStream::connect(&member.handover_endpoint))
            .await
            .context("connecting timed out")?
            .context("connecting")?;
        send(&mut stream, &Ask { nonce: hex::encode(nonce), statement: *statement, signature }).await?;
        let answer: Answer = receive(&mut stream).await?;
        let now = self.trusted.since_sample_ms().map(|ms| ms / 1000);
        let checks = Checks { domain: self.domain, provider: self.provider, own: &self.platform, now_seconds: now };
        admit(&checks, (&nonce, statement), member, excluded, answer)
    }

    /// Gives the verification by `candidate`'s member on `release` to the sequencer: whether it took it.
    /// Error cases:
    /// - the sequencer has stopped
    async fn give(&self, due: &Due, candidate: AccountId, release: B256) -> anyhow::Result<bool> {
        let sequencer = self.sequencer.upgrade().context("the sequencer has stopped")?;
        let (reply, taken) = oneshot::channel();
        sequencer
            .send(Request::LogChecked { head: due.head, candidate, release, reply })
            .await
            .ok()
            .context("the sequencer has stopped")?;
        let taken = taken.await.context("the sequencer has stopped")?;
        if !taken {
            error!(release = %release, "the sequencer refused a verification its node admitted");
        }
        Ok(taken)
    }
}

/// What a verification is checked against: this deployment, this enclave's quote provider and
/// platform, and trusted time in seconds, which a hardware quote's chain is checked at.
pub(crate) struct Checks<'a> {
    pub domain: CheckpointDomain,
    pub provider: TeeProvider,
    pub own: &'a Platform,
    pub now_seconds: Option<i64>,
}

/// The members of `order` the node asks for `due`'s check, in order: not its own candidate, with a
/// handover endpoint, and neither the member nor its release having sequenced any of the covered range.
pub(crate) fn qualifying<'a>(order: &'a [HandoverSource], due: &'a Due) -> impl Iterator<Item = &'a HandoverSource> {
    order.iter().filter(|member| {
        member.candidate != due.candidate
            && !member.handover_endpoint.is_empty()
            && !due.excluded.include(member.candidate, B256::from(member.release))
    })
}

/// The release `member`'s `answer` to `nonce` and `statement` proves, when it counts as the check:
/// its quote binds them and the config hash it names, under a guest policy and TCB no weaker than
/// this enclave's, and the measurement with that config is the release `member`'s candidacy
/// registered, neither of which sequenced any of the covered range (`excluded`).
/// Error cases:
/// - the member refused, or any of those checks fails
pub(crate) fn admit(
    checks: &Checks<'_>,
    (nonce, statement): (&[u8; 32], &Statement),
    member: &HandoverSource,
    excluded: &Sequencers,
    answer: Answer,
) -> anyhow::Result<B256> {
    let (config_sha256, evidence) = match answer {
        Answer::Verified { config_sha256, evidence } => (config_sha256, evidence),
        Answer::Refused { reason } => bail!("the member refused: {reason}"),
    };
    let config_sha256: [u8; 32] = hex::decode(&config_sha256)
        .ok()
        .and_then(|config| config.try_into().ok())
        .context("the config hash is not 32 bytes of hex")?;
    let bound = report_data(checks.domain, nonce, statement, &config_sha256);
    let peer = evidence::verify(checks.provider, &evidence, &bound, checks.now_seconds, None)?;
    no_weaker(checks.own, &peer)?;
    let release = B256::from(Release { launch_digest: peer.launch_digest, config_sha256 }.id());
    anyhow::ensure!(
        release == B256::from(member.release),
        "the quote proves release {release}, not {}, which candidate {} registered",
        B256::from(member.release),
        member.candidate
    );
    anyhow::ensure!(
        !excluded.include(member.candidate, release),
        "candidate {} or its release {release} sequenced part of the range it would verify",
        member.candidate
    );
    Ok(release)
}

#[cfg(test)]
mod tests;
