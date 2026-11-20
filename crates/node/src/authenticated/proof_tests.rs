//! A proof's reads load only the children upstream's proof can ask for, and every read still proves its value.

use anyhow::Result;

use crate::authenticated::Root;
use crate::authenticated::reader::proof_children;
use crate::authenticated::tests::{fresh, put};

/// For every set of children and every nibble, the children a proof loads hold every child upstream's
/// proof reads: each sibling alone in its range, which it reads infallibly, and the child it descends to.
/// With every child a leaf, upstream reads the most.
#[test]
fn a_proof_loads_every_child_upstreams_proof_can_read() {
    for existing in 1..=u16::MAX {
        for nibble in 0..16u8 {
            let reads = upstream_reads(existing, existing, nibble);
            assert_eq!(reads & !proof_children(existing, nibble), 0, "{existing:016b} at {nibble}");
        }
    }
}

/// Reads of present and absent keys through trees of every depth the fixture reaches give each value,
/// and absence, as they did when every child was loaded.
#[test]
fn reads_through_the_proofs_children_prove_every_value_and_absence() -> Result<()> {
    let (_dir, store) = fresh()?;
    let mut root = Root::empty([41; 32]);
    for round in 0..3u32 {
        root = store.update(
            &root,
            (round * 400..(round + 1) * 400).map(|n| put(&n.to_be_bytes(), &n.to_le_bytes())).collect(),
        )?;
    }
    for n in 0..1_400u32 {
        let expected = (n < 1_200).then(|| n.to_le_bytes().to_vec());
        assert_eq!(store.read(&root, &n.to_be_bytes())?, expected, "key {n}");
    }
    Ok(())
}

// ── Helpers ──

/// The children upstream's proof of the path through `nibble` reads, from its own rule: from the widest
/// range down, the sibling range's child when it is alone there and a leaf or the range is one wide, and
/// then either no child, or the child range's child when it is alone there and a leaf or the range is one
/// wide, which it descends to.
fn upstream_reads(existing: u16, leaves: u16, nibble: u8) -> u16 {
    let mut reads = 0;
    for height in (0..4u8).rev() {
        let width = 1u32 << height;
        let child_start = (0xffu8 << height) & nibble;
        let range = |start: u8| {
            let mask = (((1u32 << width) - 1) << start) as u16;
            (existing & mask, leaves & mask)
        };
        let (sibling, sibling_leaves) = range(child_start ^ (1 << height));
        if sibling != 0 && (width == 1 || (sibling.count_ones() == 1 && sibling_leaves != 0)) {
            reads |= sibling;
        }
        let (child, child_leaves) = range(child_start);
        if child == 0 {
            return reads;
        }
        if width == 1 || (child.count_ones() == 1 && child_leaves != 0) {
            return reads | child;
        }
    }
    unreachable!("the one-wide range ends the walk")
}
