//! The signed parent chain a start from a cache checks from the cache's base to the vault's finalized
//! checkpoint over the suffix it replays.

use super::*;

/// Over the suffixes a start from a cache can replay, the chain holds only when each signed checkpoint
/// up to the anchor names the one before it, from the base on, and the last of them is the anchor: one
/// that names another parent, repeats the one before it or follows a dropped one breaks it, and so does
/// a suffix that stops short of the anchor; a checkpoint past the anchor is not the chain's, and an
/// anchor below the base, a base with no parent, or no anchor checks nothing.
#[test]
fn the_chain_holds_only_from_the_base_to_the_anchor() {
    let before = checkpoint(5, 9, None);
    let base = checkpoint(10, 1, Some(&before));
    let c20 = checkpoint(20, 2, Some(&base));
    let c30 = checkpoint(30, 3, Some(&c20));
    let c40 = checkpoint(40, 4, Some(&c30));
    let at = |checkpoint: &Checkpoint| Some(link(checkpoint).unwrap());
    let cases: [(&str, &Checkpoint, Option<CheckpointParent>, Vec<&Checkpoint>, Option<bool>); 10] = [
        ("whole", &base, at(&c30), vec![&c20, &c30, &c40], Some(true)),
        ("anchor at the base", &base, at(&base), vec![&c20, &c30], Some(true)),
        ("repeated", &base, at(&c30), vec![&c20, &c20, &c30], Some(false)),
        ("dropped", &base, at(&c30), vec![&c30], Some(false)),
        ("another parent", &base, at(&c30), vec![&c30, &c20], Some(false)),
        ("stops short", &base, at(&c30), vec![&c20, &c40], Some(false)),
        ("ends before the anchor", &base, at(&c30), vec![&c20], Some(false)),
        ("anchor below the base", &base, at(&before), vec![&c40], None),
        ("base with no parent", &before, at(&c30), vec![&c20], None),
        ("no anchor", &base, None, vec![&c20], None),
    ];
    for (name, base, anchor, suffix, holds) in cases {
        let checked = AnchorChain::new(base, anchor).unwrap().map(|mut chain| {
            suffix.iter().try_for_each(|checkpoint| chain.take(&LogEntry::Checkpoint((*checkpoint).clone())))?;
            chain.finish()
        });
        assert_eq!(checked.map(|result| result.is_ok()), holds, "{name}");
    }
}

// ── Helpers ──

/// A checkpoint at `seq` whose root is all `byte`, naming `parent` as its parent, or none.
fn checkpoint(seq: u64, byte: u8, parent: Option<&Checkpoint>) -> Checkpoint {
    Checkpoint {
        parent: parent.map(|parent| link(parent).unwrap()),
        seq,
        root: format!("0x{}", hex::encode([byte; 32])),
        signature: format!("0x{}", "00".repeat(65)),
        signer: format!("0x{}", "11".repeat(20)),
        domain: None,
        deferred_withdrawals: false,
    }
}
