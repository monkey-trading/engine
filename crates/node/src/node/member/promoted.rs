//! A promoted member's start (#595). Once the schedule's promotion makes its standby key the vault's
//! signer, in a block the custody quorum agrees on, the member starts sequencing in process from its
//! copy, only if the copy holds both prefixes the promotion fixed: the outgoing epoch's latest head,
//! matched by link, and the vault's latest accepted checkpoint, matched by root. First it takes what
//! the members hold past its copy: from the member whose enclave confirmed the head, then from every
//! other member of the turn's set, round after round while one gives entries, then from the outgoing
//! operator's archive and its mirrors, each entry checked against the copy's seal chain, so it keeps
//! every command a member confirmed and its start check finds no member holding more. If it still
//! lacks a prefix, it raises Critical `promotion_start_refused` naming the range and keeps following,
//! so the next promotion passes it. It records the promotion beside its keys (`promotion.json`), reseals the standby key as
//! the turn's keys under the promotion's generation before its first command, takes the certificate
//! its standby obtained for its TLS key into its certificate store, and starts.
//!
//! A restart while the record stands resumes from the copy (`resume_promoted`), never through the cut
//! back to the vault's anchor a takeover makes: while the schedule at the latest agreed block records
//! the same promotion, with the same commitments, the vault names the key in the promotion's epoch, and
//! the registry names this release in the promotion's generation, it opens the resealed keys and starts
//! from its copy as the first start did; a release or generation moved on ends this release's term. A
//! promotion the latest block no longer records keeps it stopped until a final block passes the
//! promotion's time: then, if the final block records it, it resumes; if not, it quarantines what it
//! sequenced under it, setting the copy, the record and the resealed keys aside so nothing reopens
//! them, raises Critical `promotion_reorged`, and starts over as a member with a new standby key. A
//! promotion whose epoch the vault has since left ended as any turn does: the member follows, and the
//! record goes once a final block at or past the start's records the promotion.

use std::collections::BTreeSet;
use std::path::Path;

use alloy::primitives::{Address, B256};
use anyhow::Context;
use engine_core::types::AccountId;
use futures_util::future::BoxFuture;
use rcgen::PublicKeyData as _;
use sha2::{Digest as _, Sha256};
use tracing::{debug, error, info, warn};

use crate::checkpoints::CheckpointChain;
use crate::node::member::standby::Standby;
use crate::node::member::turn::Opened;
use crate::node::member::{Member, PostedHead, Replica, Sampling};
use crate::node::ready::{Moved, Ready, Start};
use crate::sequencer::replay::LiveReplay;
use crate::standby_key::chain::{Promotion, StandbyDomain};
use crate::tee::sealing::KeyStore;

/// What a start finds of an earlier promotion's record.
pub(super) enum Resumed {
    /// No record, or one whose term has ended: the member follows.
    None,
    /// The same promotion stands: the member starts from its copy.
    Start(Box<Start>),
    /// The latest block lacks the promotion and no final block has passed it: the member waits.
    Wait,
}

impl<C: CheckpointChain, H: PostedHead, T: Sampling> Member<'_, C, H, T> {
    /// Starts this member as the operator `promotion` made it, from `opened`, its copy and replica.
    /// Error cases:
    /// - the copy lacks a prefix the promotion fixed once every member was asked for the gap (Critical
    ///   `promotion_start_refused`), or the reseal, the record or the start's reads fail
    pub(super) async fn start_promoted(
        &mut self,
        standby: Standby,
        promotion: Promotion,
        opened: Opened,
        operator: Option<&crate::standby::Followed>,
    ) -> anyhow::Result<Start> {
        let (mut follower, mut stats, mut history, mut marks) = opened;
        let before = follower.replica().last_seq().unwrap_or(0);
        let (deadline, mut refused) = (tokio::time::Instant::now() + gap::BUDGET, BTreeSet::new());
        while let Some(source) =
            Box::pin(gap::fetch(&standby, &promotion, &mut follower, operator, (deadline, &refused))).await
        {
            // A refused entry may have left the replica ahead of the copy: it is built again from the copy.
            warn!(?source, "the copy refused a source's entries for the gap: asking the others on a rebuilt replica");
            refused.insert(source);
            (follower, stats, history, marks) = self.open(None).await?;
        }
        // The vault's checkpoint at the copy's tip may be one no member received: the copy's state
        // there anchors the start when its root is the vault's.
        if follower.replica().last_seq() == Some(promotion.checkpoint_seq) {
            follower.capture_tip();
        }
        if let Some(missing) = missing(follower.replica(), &promotion) {
            error!(
                alert = "promotion_start_refused",
                turn = promotion.turn,
                position = promotion.position,
                "the promoted member cannot start: its copy lacks {missing}, which no member could give it; the next \
                 promotion passes it"
            );
            standby.stop().await;
            anyhow::bail!("the copy lacks {missing}");
        }
        let tip = follower.replica().last_seq().unwrap_or(0);
        let candidate = standby.candidate();
        let dir = self.cfg.tee.sealing.as_ref().context("a member seals its keys")?.dir.clone();
        Record::new(candidate, standby.address(), promotion, tip + 1).write(&dir)?;
        let keys = standby.into_keys().await?;
        // Resealed before the first command, so a restart opens the key as the turn's keys only.
        let store = keys.reseal(promotion.generation)?;
        self.launch(store, promotion, candidate, (follower, stats, history, marks), before).await
    }

    /// Starts this member, the operator `promotion` made it, from its copy in `opened` under the
    /// resealed keys in `store`, `before` being the copy's tip when the promotion was seen.
    /// Error cases:
    /// - the start's reads fail (see `Ready::read_promoted`)
    async fn launch(
        &mut self,
        store: KeyStore,
        promotion: Promotion,
        candidate: AccountId,
        (follower, stats, history, marks): Opened,
        before: u64,
    ) -> anyhow::Result<Start> {
        let tip = follower.replica().last_seq().unwrap_or(0);
        let signer = AccountId(store.keys().enclave_key()?.address());
        // Before the start's reads open the member's certificate store, a restart's included.
        let tls_key_sha256 = Sha256::digest(store.keys().tls_key()?.subject_public_key_info()).into();
        crate::node::member::standby::certificate::promoted(self.cfg, &self.start, &tls_key_sha256);
        let held = (promotion.turn, promotion.position, promotion.carried);
        let reassigned = crate::node::log_check::read_reassigned(&self.vault, promotion.turn, Some(held)).await?;
        let read = Ready::read_promoted(
            self.cfg,
            &self.start,
            &self.metrics,
            signer,
            candidate,
            follower.replica(),
            promotion,
            self.time.sample(),
        );
        let mut ready = read.await?;
        ready.reassigned = Some(reassigned);
        info!(turn = promotion.turn, position = promotion.position, %signer, after = tip,
            "promoted: starting from the copy under the standby key");
        let moved = Moved { after: before, entries: tip.saturating_sub(before), bytes: 0 };
        Ok(Start { replica: Replica { follower, stats, history, marks }, store, ready, moved })
    }

    /// A start's check for an earlier promotion's record (see the module doc): the start from the copy
    /// while the same promotion stands, a wait while the chain has not settled it, and otherwise the
    /// record cleared, or the promotion's history quarantined, for the member to follow.
    /// Error cases:
    /// - the record, the chain, the resealed keys or the copy cannot be read; the caller waits
    pub(super) async fn resume_promoted(&mut self) -> anyhow::Result<Resumed> {
        let sealing = self.cfg.tee.sealing.as_ref().context("a member seals its keys")?;
        let Some(record) = Record::load(&sealing.dir)? else { return Ok(Resumed::None) };
        let key_file = crate::tee::sealing::file_for(&sealing.dir, &self.release, record.generation);
        if record.quarantining {
            quarantine(&self.cfg.node.log_dir, &sealing.dir, &key_file, &record)?;
            return Ok(Resumed::None);
        }
        let opened = KeyStore::open(&sealing.dir, self.sealer.as_ref(), self.release.config_sha256, record.generation)?;
        let Some(store) =
            opened.filter(|store| store.keys().enclave_key().is_ok_and(|key| key.address() == record.key.0))
        else {
            // The reseal never completed: the standby seal still holds the key, and opens as the
            // promotion's while it stands (`Standby::open`).
            return Ok(Resumed::None);
        };
        let log_heads = self.cfg.log_heads.as_ref().context("[log_heads]")?.contract;
        let chain = crate::custody::StandbyReader::connect_named(&self.cfg.custody, log_heads, "promotion")?;
        let vault = Address::from(self.cfg.custody.vault.parse::<AccountId>()?.0);
        let read: BoxFuture<'_, anyhow::Result<Standing>> = Box::pin(async {
            let domain = StandbyDomain::read(&chain, self.cfg.custody.chain_id, vault).await?;
            record.standing_at_start(&chain, &domain, B256::from(self.release.id()), &sealing.dir).await
        });
        let promotion = record.promotion();
        let dirs = (self.cfg.node.log_dir.as_path(), sealing.dir.as_path(), key_file.as_path());
        if let Some(resumed) = settle(read.await?, &record, dirs)? {
            return Ok(resumed);
        }
        // The promotion's fixed head and checkpoint stay checkable however far the copy has run past.
        let keep = [promotion.head_seq, promotion.checkpoint_seq];
        let opened = self.open_admitting(None, Some(record.key), &keep).await?;
        if let Some(missing) = missing(opened.0.replica(), &promotion) {
            anyhow::bail!("the promoted copy lacks {missing}");
        }
        let before = opened.0.replica().last_seq().unwrap_or(0);
        info!(
            turn = record.turn,
            position = record.position,
            from_seq = record.from_seq,
            "the promotion stands: resuming from the copy"
        );
        let start = self.launch(store, promotion, record.candidate, opened, before).await?;
        Ok(Resumed::Start(Box::new(start)))
    }
}

/// What a start does with an earlier promotion's `record` whose promotion the chain shows `standing`,
/// with the copy in `log_dir`, the sealing directory `sealing` and the resealed keys in `key`: `None`
/// while it stands, for the start to resume from the copy; otherwise follow or wait, and once a reorg
/// removed it, follow with its history quarantined.
/// Error cases:
/// - the record cannot be removed, or the quarantine fails
fn settle(
    standing: Standing,
    record: &Record,
    (log_dir, sealing, key): (&Path, &Path, &Path),
) -> anyhow::Result<Option<Resumed>> {
    match standing {
        Standing::Stands => Ok(None),
        Standing::Ended => {
            info!(
                turn = record.turn,
                "the promotion's signer epoch has ended, or the registry names another release or generation: the \
                 member follows"
            );
            std::fs::remove_file(sealing.join(RECORD)).context("removing the promotion record")?;
            Ok(Some(Resumed::None))
        }
        Standing::Passed => {
            info!(
                turn = record.turn,
                "the promotion's term has ended at the latest block, before a final block records it: the member \
                 follows and keeps the record until one does"
            );
            Ok(Some(Resumed::None))
        }
        Standing::Unsettled => {
            warn!(
                turn = record.turn,
                position = record.position,
                "the latest block no longer records the promotion that made this key the vault's signer, and no final \
                 block has settled it: waiting, signing nothing"
            );
            Ok(Some(Resumed::Wait))
        }
        Standing::Behind => {
            debug!(turn = record.turn, "the latest agreed block is older than the promotion's: waiting");
            Ok(Some(Resumed::Wait))
        }
        Standing::Gone => {
            quarantine(log_dir, sealing, key, record)?;
            Ok(Some(Resumed::None))
        }
    }
}

/// Sets aside what a promotion a reorg removed left: the copy, which holds what this member sequenced
/// under it, the resealed keys and the record, each renamed beside where it was, so no start reopens
/// them and the member rebuilds its copy from the archive. Raises Critical `promotion_reorged`.
/// Error cases:
/// - a rename fails
fn quarantine(log_dir: &Path, sealing: &Path, key: &Path, record: &Record) -> anyhow::Result<()> {
    // The phase is recorded first, so a start after a crash at any rename below finishes the rest.
    if !record.quarantining {
        Record { quarantining: true, ..*record }.write(sealing)?;
    }
    // The promotion's block time keeps a later promotion at the same assignment from reusing the name.
    let suffix = format!("quarantined-{}-{}-{}", record.turn, record.position, record.at);
    let aside = |path: &Path| -> anyhow::Result<()> {
        let mut renamed = path.as_os_str().to_owned();
        renamed.push(format!(".{suffix}"));
        match std::fs::rename(path, &renamed) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("setting {} aside", path.display())),
        }
    };
    let sync = |dir: &Path| std::fs::File::open(dir).and_then(|dir| dir.sync_all());
    aside(key)?;
    sync(sealing).context("syncing the sealing directory")?;
    aside(log_dir)?;
    if let Some(parent) = log_dir.parent() {
        sync(parent).context("syncing the log's directory")?;
    }
    // The record goes last: until it does, every start finishes the quarantine.
    aside(&sealing.join(RECORD))?;
    sync(sealing).context("syncing the sealing directory")?;
    error!(
        alert = "promotion_reorged",
        turn = record.turn,
        position = record.position,
        "a final block no longer records the promotion that made this key the vault's signer: the history \
         sequenced from seq {} is quarantined beside the log ({suffix}), the key is set aside, and the member \
         registers a new standby key",
        record.from_seq
    );
    Ok(())
}

/// What the replica lacks of `promotion`'s prefixes, if anything: the outgoing epoch's latest head,
/// matched by link, and the vault's latest accepted checkpoint, matched by root.
pub(crate) fn missing(replica: &LiveReplay, promotion: &Promotion) -> Option<String> {
    let last = replica.last_seq().unwrap_or(0);
    if promotion.head_seq > 0 {
        match replica.link_at(promotion.head_seq) {
            Some(link) if link.link == promotion.head_link => {}
            Some(link) => {
                return Some(format!(
                    "the head at seq {} (it holds link {} there, not {})",
                    promotion.head_seq, link.link, promotion.head_link
                ));
            }
            None if last < promotion.head_seq => {
                return Some(format!("seqs {}..={} up to the outgoing epoch's head", last + 1, promotion.head_seq));
            }
            None => return Some(format!("the link at the outgoing epoch's head, seq {}", promotion.head_seq)),
        }
    }
    if promotion.checkpoint_seq > 0 {
        let root = format!("{:#x}", promotion.checkpoint_root);
        match replica.checkpoint_at(promotion.checkpoint_seq) {
            Some(checkpoint) if checkpoint.root.eq_ignore_ascii_case(&root) => {}
            Some(checkpoint) => {
                return Some(format!(
                    "the checkpoint at seq {} (it holds root {}, not {root})",
                    promotion.checkpoint_seq, checkpoint.root
                ));
            }
            None if last < promotion.checkpoint_seq => {
                return Some(format!(
                    "seqs {}..={} up to the vault's latest checkpoint",
                    last + 1,
                    promotion.checkpoint_seq
                ));
            }
            None => return Some(format!("the vault's latest checkpoint at seq {}", promotion.checkpoint_seq)),
        }
    }
    None
}

mod gap;
mod record;
pub(crate) use record::{RECORD, Record, Standing};
#[cfg(test)]
mod tests;
pub(crate) mod watch;
