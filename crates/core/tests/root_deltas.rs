//! A detached state tree fed in parts gives the inline roots and proofs. Every reference log replays
//! twice: inline, as replay computes roots, and with its tree detached at the first root under `V9`,
//! fed random parts of what changed after each command (`Engine::take_root_delta`) and committed at
//! random commands (`Engine::commit_inputs`), as a node computes roots on another thread. Each commit
//! must give the root the log recorded, and every account, entitlement, distribution and exit proof
//! from it must equal the inline engine's. The logs walk every protocol version's activation with
//! state in place, `V27`'s, `V38`'s and `V43`'s discards of the tree among them.

mod common;

use std::fs;
use std::path::Path;

use common::reference::{ReferenceEntry, root_hex};
use engine_core::Engine;
use engine_core::merkle::tree::{StateTree, Version};
use engine_core::merkle::{ExitProof, TreeEvent};
use engine_core::types::AccountId;

#[test]
fn a_tree_fed_in_parts_gives_every_reference_root_and_the_inline_proofs() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference");
    let mut logs: Vec<_> = fs::read_dir(&dir).expect("reference directory").map(|e| e.expect("entry").path()).collect();
    logs.retain(|path| path.extension().is_some_and(|ext| ext == "jsonl"));
    logs.sort();
    let mut totals = Totals::default();
    for (index, path) in logs.iter().enumerate() {
        let entries: Vec<ReferenceEntry> = fs::read_to_string(path)
            .expect("readable log")
            .lines()
            .map(|line| serde_json::from_str(line).expect("a reference entry"))
            .collect();
        replay_in_parts(&entries, 0x925 + index as u64, &mut totals)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
    // The logs reach every case the parts must survive.
    assert!(totals.commits > 1_000 && totals.parts > 1_000, "{totals:?}");
    assert!(totals.resets > 50 && totals.discards >= 3, "{totals:?}");
    assert!(totals.exit_proofs > 100 && totals.approvals > 10, "{totals:?}");
}

// ── Helpers ──

/// What the replays went through.
#[derive(Debug, Default)]
struct Totals {
    commits: usize,
    parts: usize,
    resets: usize,
    discards: usize,
    exit_proofs: usize,
    approvals: usize,
}

/// Replays `entries` inline and with a detached tree fed in parts, comparing each commit's root with
/// the recorded one and its proofs with the inline engine's.
/// Error cases:
/// - a commit's root differs from the recorded root
/// - a proof from the detached tree differs from the inline engine's
fn replay_in_parts(entries: &[ReferenceEntry], seed: u64, totals: &mut Totals) -> Result<(), String> {
    let mut rng = Rng(seed);
    let (mut inline, mut parted) = (Engine::new(), Engine::new());
    let mut tree: Option<StateTree> = None;
    for (at, entry) in entries.iter().enumerate() {
        let seq = entry.sequenced.seq;
        inline.apply(&entry.sequenced).map_err(|e| format!("seq {seq}: {e}"))?;
        parted.apply(&entry.sequenced).map_err(|e| format!("seq {seq}: {e}"))?;
        let root = inline.state_root();
        let Some(tree) = &mut tree else {
            // Before the first root under `V9` a node computes roots inline, then takes the tree.
            parted.state_root();
            tree = parted.detach_tree();
            if tree.as_ref().is_some_and(|tree| tree.released().is_some()) {
                return Err(format!("seq {seq}: a tree is handed over with a version released"));
            }
            continue;
        };
        match parted.take_tree_event() {
            Some(TreeEvent::Reset) => {
                tree.reset();
                totals.resets += 1;
            }
            Some(TreeEvent::Discard) => {
                *tree = StateTree::default();
                totals.discards += 1;
            }
            None => {}
        }
        for _ in 0..rng.below(3) {
            parted.take_root_delta(1 + rng.below(48) as usize).apply(tree);
            totals.parts += 1;
        }
        if rng.below(3) > 0 && at + 1 < entries.len() {
            continue;
        }
        let (committed, _) = parted.commit_inputs().apply(tree);
        if root_hex(committed.0) != entry.root {
            return Err(format!("seq {seq}: a commit gives {committed}, the log {}", entry.root));
        }
        if !tree.release(seq, committed) {
            return Err(format!("seq {seq}: no version to release"));
        }
        tree.collect(usize::MAX);
        totals.commits += 1;
        let version = tree.released().expect("released above");
        for account in inline.accounts().keys().copied() {
            same_proofs(&inline, tree, version, account, totals).map_err(|e| format!("seq {seq}, {account}: {e}"))?;
        }
        if root != committed.0 {
            return Err(format!("seq {seq}: the inline root differs"));
        }
    }
    Ok(())
}

/// Whether `account`'s proofs from `version` equal those `inline` gives at the same root.
fn same_proofs(
    inline: &Engine,
    tree: &StateTree,
    version: &Version,
    account: AccountId,
    totals: &mut Totals,
) -> Result<(), String> {
    if tree.account_proof(version, account) != inline.account_proof(account) {
        return Err("account proofs differ".into());
    }
    if tree.entitlement_proof(version, account) != inline.entitlement_proof(account) {
        return Err("entitlement proofs differ".into());
    }
    if tree.distribution_proof(version, account) != inline.distribution_proof(account) {
        return Err("distribution proofs differ".into());
    }
    let parted = exit_proof(tree, version, account);
    if parted != inline.exit_proof(account) {
        return Err(format!("exit proofs differ: {parted:?} against {:?}", inline.exit_proof(account)));
    }
    if let Some(proof) = parted {
        totals.exit_proofs += 1;
        totals.approvals += proof.approvals.len();
        if !proof.approvals.iter().all(|approval| approval.verify(version.root())) {
            return Err("an approval does not verify".into());
        }
    }
    Ok(())
}

/// `account`'s exit proof from `version`, its approvals read one by one, as a node's reads take them.
fn exit_proof(tree: &StateTree, version: &Version, account: AccountId) -> Option<ExitProof> {
    let mut proof = tree.exit_proof_without_approvals(version, account)?;
    let nonces = proof.account.as_ref().map_or(0, |proof| proof.leaf.withdrawal_nonce);
    proof.approvals = (1..=nonces).filter_map(|nonce| tree.approval_proof(version, account, nonce)).collect();
    Some(proof)
}

/// A xorshift generator, so each log's parts and commits fall the same way on every run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}
