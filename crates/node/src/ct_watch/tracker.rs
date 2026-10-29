//! What the watcher has seen and decided: the attested key hashes, how far each log is read, and
//! the certificates for the domain whose key no verified attestation has named. It does no IO and
//! reads no clock: callers pass the time, and read `state` to persist it after a change.
//!
//! A certificate for an unknown key is held for `grace` after its log time, because a restarted
//! node gets its certificate before the watcher polls its new attestation. If an attestation
//! names the key by then, the certificate is dropped without an alert; if not, it raises the
//! Critical `unattested_certificate` and asserts it on every check until an attestation names the
//! key or the certificate expires; the router collapses the repeats into one episode, and a page
//! lost before the outbox committed it is paged by the next one.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::time::Duration;

use prometheus_client::metrics::counter::Counter;
use tracing::{error, info};

use crate::ct_watch::leaf::{Certificate, covers};
use crate::ct_watch::state::{Finding, LogProgress, MAX_ATTESTED, MAX_FINDINGS, State};

/// The watcher's state and the rules that move it.
pub(super) struct Tracker {
    domain: String,
    grace_ms: u64,
    stale_after_ms: u64,
    pub(super) state: State,
    changed: bool,
    /// Each configured log's name and the fingerprint of its key, which keys its progress.
    logs: BTreeMap<String, String>,
    /// Logs whose outage alert is still open under a label they no longer have: the fingerprint of
    /// each and that label. A resolve is sent until the alert queue accepts it.
    renamed: Vec<(String, String)>,
    /// Counts the alert lines the router's queue refused; a resolve that raises it was not accepted.
    drops: Counter,
    /// Set until the first `settle`, which resolves again the alerts a previous run was resolving:
    /// the resolve may not have reached the outbox before a kill.
    resuming: bool,
}

impl Tracker {
    /// Resumes from `state`, giving every log in `logs` (its name and key fingerprint) a progress
    /// entry that counts from `now`.
    pub(super) fn new(
        mut state: State,
        domain: &str,
        grace: Duration,
        log_stale_after: Duration,
        logs: impl IntoIterator<Item = (String, String)>,
        now: u64,
    ) -> Self {
        let mut changed = false;
        let logs: BTreeMap<String, String> = logs.into_iter().collect();
        // Progress belongs to a log's key: it survives a rename or a new URL, and a log taken out
        // of the config keeps its position for when it returns. A new shard (a new key) starts
        // afresh. A log not configured is not checked, and an outage alert raised under a label it
        // no longer has is resolved.
        let mut renamed = Vec::new();
        for (fingerprint, progress) in &state.logs {
            let configured = logs.iter().find(|(_, key)| *key == fingerprint).map(|(name, _)| name);
            if !progress.name.is_empty() && configured != Some(&progress.name) && !logs.contains_key(&progress.name) {
                renamed.push((fingerprint.clone(), progress.name.clone()));
            }
        }
        for (name, fingerprint) in &logs {
            state.logs.entry(fingerprint.clone()).or_insert_with(|| {
                changed = true;
                LogProgress { name: name.clone(), next_index: None, caught_up_ms: now }
            });
        }
        if state.last_verified_ms == 0 {
            state.last_verified_ms = now;
            changed = true;
        }
        Self {
            domain: domain.to_owned(),
            grace_ms: millis(grace),
            stale_after_ms: millis(log_stale_after),
            state,
            changed,
            logs,
            renamed,
            drops: Counter::default(),
            resuming: true,
        }
    }

    /// Reads the alert queue's refusals from `drops`, so a resolve it refused is sent again.
    pub(super) fn count_drops(&mut self, drops: Counter) {
        self.drops = drops;
    }

    /// Whether the state changed since the last call; clears the flag.
    pub(super) fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    /// Records the TLS key hash a verified attestation named, evicting the oldest past the bound.
    pub(super) fn attested(&mut self, key_sha256: &[u8; 32], now: u64) {
        self.state.last_verified_ms = now;
        self.changed = true;
        let attested = &mut self.state.attested;
        if let Entry::Vacant(first_seen) = attested.entry(hex::encode(key_sha256)) {
            first_seen.insert(now);
            self.changed = true;
            info!(key_sha256 = %hex::encode(key_sha256), "new attested TLS key");
        }
        while attested.len() > MAX_ATTESTED {
            let oldest = attested.iter().min_by_key(|(_, seen)| **seen).map(|(key, _)| key.clone());
            attested.remove(&oldest.expect("a non-empty map has an oldest key"));
        }
    }

    /// The next entry to read from `log`, fixing the starting point on the first tree head: the
    /// configured `start_index`, or the tree's end. A start past the end is read from the end.
    pub(super) fn next_index(&mut self, log: &str, tree_size: u64, start_index: Option<u64>) -> u64 {
        let progress = self.state.logs.get_mut(&self.logs[log]).expect("every configured log has progress");
        let first = progress.next_index.is_none();
        let next = *progress.next_index.get_or_insert(start_index.unwrap_or(tree_size).min(tree_size));
        self.changed |= first;
        next
    }

    /// Checks one entry of `log`: a certificate for the domain whose key is not attested is held
    /// for its grace wait. The caller has checked `lacks_room_for` its batch.
    pub(super) fn ingest(&mut self, log: &str, index: u64, cert: &Certificate, now: u64) {
        if !self.is_unattested_match(cert, now) {
            return;
        }
        let key_sha256 = hex::encode(cert.key_sha256);
        // A log cannot hold a certificate until later than it is read.
        let decide_at_ms = cert.logged_ms.min(now).saturating_add(self.grace_ms);
        let mut finding = Finding {
            log: log.to_owned(),
            index,
            issuer: cert.issuer.clone(),
            key_sha256,
            not_after_ms: cert.not_after_ms,
            decide_at_ms,
            alerted_at_ms: None,
            resolving: false,
        };
        info!(log, index, key_sha256 = %finding.key_sha256, decide_at_ms, "certificate for the domain has an unknown key");
        if now >= decide_at_ms {
            raise(&finding);
            finding.alerted_at_ms = Some(now);
        }
        self.state.findings.push(finding);
        self.changed = true;
    }

    /// Whether holding every certificate of `batch` that needs it would pass `MAX_FINDINGS`. The log
    /// then waits, so no finding is dropped and each keeps its grace and its retries.
    pub(super) fn lacks_room_for<'a>(&self, batch: impl IntoIterator<Item = &'a Certificate>, now: u64) -> bool {
        let needed = batch.into_iter().filter(|cert| self.is_unattested_match(cert, now)).count();
        self.state.findings.len() + needed > MAX_FINDINGS
    }

    /// Whether `cert` names the domain, has not expired at `now`, and has an unattested key.
    fn is_unattested_match(&self, cert: &Certificate, now: u64) -> bool {
        covers(&cert.names, &self.domain)
            && cert.not_after_ms > now
            && !self.state.attested.contains_key(&hex::encode(cert.key_sha256))
    }

    /// Records that `log` is read up to entry `next_index`. `read_to_head` is the timestamp of the
    /// signed tree head the whole tree was read up to, if it was: the log then counts as read as of
    /// that head, never later than `now`, so a head replayed past its age does not keep the log
    /// healthy.
    pub(super) fn advance(&mut self, log: &str, next_index: u64, read_to_head: Option<u64>, now: u64) {
        let progress = self.state.logs.get_mut(&self.logs[log]).expect("every configured log has progress");
        progress.next_index = Some(next_index);
        if let Some(head_ms) = read_to_head {
            progress.caught_up_ms = progress.caught_up_ms.max(head_ms.min(now));
            if now.saturating_sub(progress.caught_up_ms) < self.stale_after_ms {
                info!(alert_resolved = "ct_log_unreachable", log, "log read up to a recent signed tree head");
            }
        }
        self.changed = true;
    }

    /// Decides held findings whose grace has ended, asserts the open alerts again, and ends those
    /// whose key an attestation now names or whose certificate expired. An ended alert is resolved
    /// on one pass and dropped on the next, so a kill between the resolve line and its commit to the
    /// outbox still resolves it after a restart.
    pub(super) fn settle(&mut self, now: u64) {
        self.resolve_renamed();
        let drops = &self.drops;
        let State { attested, findings, .. } = &mut self.state;
        let resuming = std::mem::take(&mut self.resuming);
        let before = findings.len();
        let mut touched = false;
        findings.retain_mut(|finding| {
            let named = attested.contains_key(&finding.key_sha256);
            let expired = finding.not_after_ms <= now;
            match finding.alerted_at_ms {
                None if named || expired => false,
                Some(_) if named || expired => {
                    if finding.resolving && !resuming {
                        return false;
                    }
                    let refused = drops.get();
                    info!(
                        alert_resolved = "unattested_certificate",
                        log = %finding.log, index = finding.index, issuer = %finding.issuer,
                        key_sha256 = %finding.key_sha256,
                        "the certificate's key is attested or the certificate has expired"
                    );
                    // Forgotten only once the queue took the line: a refused resolve is sent again.
                    let accepted = drops.get() == refused;
                    touched |= accepted != finding.resolving;
                    finding.resolving = accepted;
                    true
                }
                None if now >= finding.decide_at_ms => {
                    raise(finding);
                    finding.alerted_at_ms = Some(now);
                    touched = true;
                    true
                }
                Some(_) => {
                    raise(finding);
                    true
                }
                None => true,
            }
        });
        self.changed |= touched || findings.len() != before;
    }

    /// Resolves the outage alert of each log that was renamed or taken out of the config, under the
    /// label it had, until the alert queue takes the line. A renamed log then carries its new label.
    fn resolve_renamed(&mut self) {
        let pending = std::mem::take(&mut self.renamed);
        for (fingerprint, old) in pending {
            let refused = self.drops.get();
            info!(alert_resolved = "ct_log_unreachable", log = %old, "the log is no longer followed under this name");
            if self.drops.get() != refused {
                self.renamed.push((fingerprint, old));
            } else if let Some((name, _)) = self.logs.iter().find(|(_, key)| **key == fingerprint) {
                self.state.logs.get_mut(&fingerprint).expect("a followed log has progress").name.clone_from(name);
                self.changed = true;
            }
        }
    }

    /// Asserts `attestation_unverified` while no attestation has verified for a grace: the watcher
    /// cannot tell a restart's key from a foreign one.
    pub(super) fn check_attestation(&mut self, now: u64) {
        let behind = now.saturating_sub(self.state.last_verified_ms);
        if behind >= self.grace_ms {
            error!(
                alert = "attestation_unverified",
                "no attestation of the node has verified for {}s; its keys are not being learned",
                behind / 1000
            );
        }
    }

    /// Asserts `ct_log_unreachable` for each log not read up to a signed tree head newer than
    /// `log_stale_after`.
    pub(super) fn check_logs(&mut self, now: u64) {
        for (name, fingerprint) in &self.logs {
            let behind = now.saturating_sub(self.state.logs[fingerprint].caught_up_ms);
            if behind >= self.stale_after_ms {
                error!(
                    alert = "ct_log_unreachable",
                    log = %name,
                    "log not read up to a recent signed tree head for {}s; certificates in it are not being checked",
                    behind / 1000
                );
            }
        }
    }
}

/// Raises the Critical alert for a certificate whose key no verified attestation names.
fn raise(finding: &Finding) {
    error!(
        alert = "unattested_certificate",
        log = %finding.log, index = finding.index, issuer = %finding.issuer, key_sha256 = %finding.key_sha256,
        "certificate for the API domain with a key no verified attestation names"
    );
}

/// A duration in whole milliseconds, saturating.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
