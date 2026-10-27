//! Strict-majority custody reads. Each vote compares the same block or range; an empty log
//! response counts only after that provider also returns the range's last block. Finality tags
//! can advance between requests, so providers vote at the highest height a majority declares
//! final, with its hash independently read there. Every read requires admission by the current
//! poll's identity majority; identity failure clears admission. The watcher checks that majority
//! against its configured chain/token before further reads. The configured majority threshold
//! never shrinks with admission or availability. No answer is reused after losing quorum.

use std::collections::BTreeSet;
use std::sync::Mutex;

use alloy::primitives::Address;
use anyhow::{Context, bail};
use futures_util::future::join_all;
use tracing::{error, info, warn};

use crate::config::EvmCustodySection;
use crate::custody::chain::BlockLink;
use crate::custody::evm::{BlockRef, Finality, HeadSupport, StakingEvent, VaultDeposit, VaultIdentity, VaultReader};
use crate::custody::rpc::{InvalidRpcResponse, VaultRpc};

/// Independent providers and their alert identities; the watcher owns the reader.
pub(crate) struct Quorum<R> {
    pub(super) providers: Vec<(String, R)>,
    required: usize,
    // The watcher owns this reader and polls serially. VaultReader uses shared references;
    // this lock guards synchronous admission/alert transitions and is never held across IO.
    alerts: Mutex<Alerts>,
}

#[derive(Default)]
struct Alerts {
    providers: BTreeSet<(usize, &'static str, &'static str)>,
    lost: BTreeSet<&'static str>,
    eligible: BTreeSet<usize>,
}

impl Quorum<VaultRpc> {
    /// Builds clients without accessing the network.
    /// Error cases:
    /// - fewer than three providers, or a client cannot be built
    pub(super) fn connect(cfg: &EvmCustodySection, vault: Address) -> anyhow::Result<Self> {
        let providers = cfg
            .providers
            .iter()
            .map(|p| Ok((p.name.clone(), VaultRpc::new(cfg, &p.rpc_url, vault)?)))
            .collect::<anyhow::Result<_>>()?;
        Self::new(providers)
    }
}

impl<R> Quorum<R> {
    /// Requires a strict majority of at least three configured providers.
    /// Error cases:
    /// - fewer than three providers
    pub(super) fn new(providers: Vec<(String, R)>) -> anyhow::Result<Self> {
        if providers.len() < 3 {
            bail!("custody quorum needs at least three providers");
        }
        Ok(Self { required: providers.len() / 2 + 1, providers, alerts: Mutex::default() })
    }

    /// Refuses reads until this poll's identity majority has admitted the provider.
    /// Error cases:
    /// - identity has not succeeded, failed, or excluded this provider
    fn qualified(&self, index: usize) -> anyhow::Result<()> {
        if !self.alerts.lock().expect("quorum alert lock poisoned").eligible.contains(&index) {
            bail!("provider is outside this poll's identity quorum");
        }
        Ok(())
    }

    /// Selects one identical answer with a strict majority and records alert transitions.
    /// Error cases:
    /// - insufficient identical successful responses; no value is returned or cached
    fn vote<T: Clone + PartialEq>(
        &self,
        operation: &'static str,
        answers: Vec<anyhow::Result<T>>,
    ) -> anyhow::Result<(T, Vec<usize>)> {
        let winner = majority(&answers, self.required);
        let mut alerts = self.alerts.lock().expect("quorum alert lock poisoned");
        for (index, answer) in answers.iter().enumerate() {
            let name = &self.providers[index].0;
            match answer {
                Err(cause) => {
                    if alerts.providers.insert((index, operation, "custody_rpc_provider_failed")) {
                        warn!(
                            alert = "custody_rpc_provider_failed",
                            provider = name,
                            operation,
                            "provider failed: {cause:#}"
                        );
                    }
                }
                Ok(value) => {
                    resolve(&mut alerts, index, name, operation, "custody_rpc_provider_failed");
                    if let Some(winner) = winner {
                        if value == winner {
                            resolve(&mut alerts, index, name, operation, "custody_rpc_provider_disagreed");
                        } else if alerts.providers.insert((index, operation, "custody_rpc_provider_disagreed")) {
                            warn!(
                                alert = "custody_rpc_provider_disagreed",
                                provider = name,
                                operation,
                                "provider disagrees with custody quorum"
                            );
                        }
                    }
                }
            }
        }
        let Some(value) = winner else {
            if alerts.lost.insert(operation) {
                error!(
                    alert = "custody_rpc_quorum_lost",
                    operation,
                    "custody requires {} identical votes; crediting paused, trading continues",
                    self.required
                );
            }
            bail!("custody RPC quorum lost for {operation}: need {} identical votes", self.required);
        };
        if alerts.lost.remove(operation) {
            info!(alert_resolved = "custody_rpc_quorum_lost", operation, "custody quorum restored");
        }
        let voters = answers
            .iter()
            .enumerate()
            .filter_map(|(i, a)| a.as_ref().is_ok_and(|other| other == value).then_some(i))
            .collect();
        Ok((value.clone(), voters))
    }
}

impl<R: VaultReader + Sync> VaultReader for Quorum<R> {
    /// The identity agreed by a strict majority.
    /// Error cases:
    /// - no quorum
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        self.alerts.lock().expect("quorum alert lock poisoned").eligible.clear();
        let (identity, voters) =
            self.vote("identity", join_all(self.providers.iter().map(|(_, r)| r.identity())).await)?;
        self.alerts.lock().expect("quorum alert lock poisoned").eligible.extend(voters);
        Ok(identity)
    }

    /// The highest common final height and its majority hash, with conservative head diagnostics.
    /// Error cases:
    /// - no quorum for finality or the block hash at the common height
    async fn finality(&self) -> anyhow::Result<Finality> {
        self.read_finality(false).await
    }

    /// The common withdrawal-finality height and its majority hash.
    async fn withdrawal_finality(&self) -> anyhow::Result<Finality> {
        self.read_finality(true).await
    }

    /// Identical ordered headers over the same height range.
    /// Error cases:
    /// - no quorum
    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        let answers = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
            self.qualified(i)?;
            r.headers(from, to).await
        }))
        .await;
        self.vote("headers", answers).map(|v| v.0)
    }

    /// Identical complete log sets and end-block hashes; a lagging absence cannot vote.
    /// Error cases:
    /// - no quorum, or a provider cannot return the range's last block
    async fn deposits(&self, from: u64, to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        let answers = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
            self.qualified(i)?;
            let header = last_header(r.headers(to, to).await?, to)?;
            Ok((header.hash, r.deposits(from, to).await?))
        }))
        .await;
        self.vote("deposits", answers).map(|v| v.0.1)
    }

    /// Identical ordered staking event sets; a deployment without a staking contract answers
    /// empty on every provider.
    /// Error cases:
    /// - no quorum
    async fn staking_events(&self, from: u64, to: u64) -> anyhow::Result<Vec<StakingEvent>> {
        let answers = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
            self.qualified(i)?;
            let header = last_header(r.headers(to, to).await?, to)?;
            Ok((header.hash, r.staking_events(from, to).await?))
        }))
        .await;
        self.vote("staking_events", answers).map(|v| v.0.1)
    }

    /// Identical counts at the same canonical block hash.
    /// Error cases:
    /// - no quorum
    async fn deposit_count(&self, at: BlockRef) -> anyhow::Result<u64> {
        let answers = join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
            self.qualified(i)?;
            r.deposit_count(at).await
        }))
        .await;
        self.vote("deposit_count", answers).map(|v| v.0)
    }
}

impl<R: VaultReader + Sync> Quorum<R> {
    /// Selects the highest common tag height, never reducing the fixed provider threshold.
    /// A chain move can split one poll's reads — a revert or reorg between the pair reads and
    /// the common height's header reads leaves every provider contradicting itself — so when no
    /// common height wins on evidence like that, the pairs are read once more and the selection
    /// runs again; a provider that still contradicts itself fails on the second pass as before.
    /// Error cases:
    /// - the selected tag or its canonical hash lacks a quorum
    async fn read_finality(&self, withdrawal: bool) -> anyhow::Result<Finality> {
        let answers = self.read_finality_pairs(withdrawal).await;
        let (mut votes, mut diagnostics, split) = self.vote_at_common_height(answers).await;
        if majority(&votes, self.required).is_none() && split {
            let answers = self.read_finality_pairs(withdrawal).await;
            (votes, diagnostics, _) = self.vote_at_common_height(answers).await;
        }
        let (tagged, _) = self.vote(if withdrawal { "withdrawal_finality" } else { "finality" }, votes)?;
        let head = quorum_lower_bound(diagnostics.iter().flatten().map(|d| d.0), self.required);
        let head_time = quorum_lower_bound(diagnostics.iter().flatten().map(|d| d.1), self.required);
        let head_support =
            Some(HeadSupport { required: self.required, heads: diagnostics.iter().map(|d| d.map(|d| d.0)).collect() });
        Ok(Finality { head, head_time, tagged, head_support })
    }

    /// One fresh finality pair per provider.
    async fn read_finality_pairs(&self, withdrawal: bool) -> Vec<anyhow::Result<Finality>> {
        join_all(self.providers.iter().enumerate().map(|(i, (_, r))| async move {
            self.qualified(i)?;
            if withdrawal { r.withdrawal_finality().await } else { r.finality().await }
        }))
        .await
    }

    /// Votes the providers' pairs at the highest common tag heights; a provider whose own
    /// evidence disagrees with itself is excluded from lower candidates. Returns the votes, the
    /// head diagnostics of providers whose evidence stayed consistent, and whether any provider
    /// was contradicted — the signature of the poll's reads straddling a chain move.
    async fn vote_at_common_height(
        &self,
        mut answers: Vec<anyhow::Result<Finality>>,
    ) -> (Vec<anyhow::Result<Option<BlockRef>>>, Vec<Option<(u64, u64)>>, bool) {
        // Head diagnostics are independent of how far each provider's finality tag has reached.
        let mut diagnostics: Vec<_> = answers
            .iter()
            .map(|a| {
                a.as_ref().ok().filter(|f| f.tagged.is_none_or(|b| b.number <= f.head)).map(|f| (f.head, f.head_time))
            })
            .collect();
        let mut heights: Vec<_> = answers
            .iter()
            .filter_map(|a| a.as_ref().ok().filter(|f| f.tagged.is_none_or(|b| b.number <= f.head)))
            .filter_map(|f| f.tagged.map(|b| b.number))
            .collect();
        heights.sort_unstable_by(|a, b| b.cmp(a));
        let mut candidates: Vec<_> = heights.into_iter().skip(self.required - 1).map(Some).collect();
        candidates.dedup();
        let absent = answers.iter().filter(|a| a.as_ref().is_ok_and(|f| f.tagged.is_none())).count();
        if absent >= self.required || candidates.is_empty() {
            candidates.push(None);
        }
        let mut votes = Vec::new();
        let mut split = false;
        for common in candidates {
            let results = join_all(self.providers.iter().zip(&answers).map(|((_, reader), answer)| async move {
                let mut contradicted = false;
                let vote = async {
                    let f = answer.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))?;
                    if f.tagged.is_some_and(|b| b.number > f.head) {
                        contradicted = true;
                        bail!("finality tag is above the provider head");
                    }
                    match (common, f.tagged) {
                        (Some(number), Some(tagged)) if tagged.number >= number => {
                            let header = reader
                                .headers(number, number)
                                .await
                                .and_then(|headers| last_header(headers, number))
                                .inspect_err(|cause| {
                                    contradicted |= cause.is::<InvalidRpcResponse>();
                                })?;
                            if tagged.number == number && tagged.hash != header.hash {
                                contradicted = true;
                                bail!("finality tag disagrees with the provider's header");
                            }
                            Ok(Some(BlockRef { number, hash: header.hash }))
                        }
                        (None, None) => Ok(None),
                        _ => bail!("provider has not reached the common final block"),
                    }
                }
                .await;
                (vote, contradicted)
            }))
            .await;
            for (diagnostic, (_, contradicted)) in diagnostics.iter_mut().zip(&results) {
                if *contradicted {
                    *diagnostic = None;
                    split = true;
                }
            }
            votes = results.into_iter().map(|(vote, _)| vote).collect();
            if majority(&votes, self.required).is_some() {
                break;
            }
            // A lying high tag must not prevent the honest majority agreeing at a lower height.
            // Providers that contradicted their own tag stay excluded for this poll.
            for (answer, vote) in answers.iter_mut().zip(&votes) {
                let eligible = answer.as_ref().is_ok_and(|f| match (common, f.tagged) {
                    (Some(n), Some(b)) => b.number >= n,
                    _ => false,
                });
                if eligible && let Err(cause) = vote {
                    *answer = Err(anyhow::anyhow!("{cause:#}"));
                }
            }
        }
        (votes, diagnostics, split)
    }
}

/// A diagnostic lower bound supported by the full configured majority, ignoring minority extremes.
fn quorum_lower_bound(values: impl Iterator<Item = u64>, required: usize) -> u64 {
    let mut values: Vec<_> = values.collect();
    values.sort_unstable_by(|a, b| b.cmp(a));
    *values.get(required - 1).expect("successful finality voters meet the configured threshold")
}

/// The first value with enough identical successful answers; strict majorities cannot tie.
fn majority<T: PartialEq>(answers: &[anyhow::Result<T>], required: usize) -> Option<&T> {
    answers
        .iter()
        .filter_map(|a| a.as_ref().ok())
        .find(|value| answers.iter().filter(|a| a.as_ref().is_ok_and(|other| other == *value)).count() >= required)
}

/// Requires exactly the header requested, so a lagging absence carries a verified height.
/// Error cases:
/// - a missing, extra, or wrong-height header
fn last_header(headers: Vec<BlockLink>, number: u64) -> anyhow::Result<BlockLink> {
    let header = headers.first().copied().ok_or(InvalidRpcResponse).context("provider has no requested block")?;
    if headers.len() != 1 || header.number != number {
        return Err(anyhow::Error::new(InvalidRpcResponse).context("provider returned the wrong header range"));
    }
    Ok(header)
}

/// Ends one provider's alert once that operation has a healthy response.
fn resolve(alerts: &mut Alerts, index: usize, name: &str, operation: &'static str, alert: &'static str) {
    if alerts.providers.remove(&(index, operation, alert)) {
        info!(alert_resolved = alert, provider = name, operation, "provider recovered");
    }
}
