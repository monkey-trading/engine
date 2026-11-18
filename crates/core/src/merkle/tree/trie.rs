//! The trie over the arena, laid out as the `merkle` module docs describe: a branch at every depth
//! where two or more leaves lie under its prefix, and each leaf at the depth where its key parts from
//! every other key.

use alloy_primitives::B256;

use crate::merkle::tree::arena::{Arena, Leaf, Node, NodeRef};
use crate::merkle::tree::value::LeafValue;
use crate::merkle::{EMPTY, bit, node_hash};

/// A leaf to write: its hash, and what proofs read from it.
#[derive(Debug)]
pub(crate) struct NewLeaf {
    pub(crate) hash: B256,
    pub(crate) value: Option<Box<LeafValue>>,
}

impl Arena {
    /// The leaf at `key` in the tree under `root`, if there is one.
    pub(crate) fn lookup(&self, root: NodeRef, key: &B256) -> Option<&Leaf> {
        let (mut node, mut depth) = (root, 0);
        loop {
            match node.node() {
                Node::Empty => return None,
                Node::Leaf(index) => return Some(self.leaf(index)).filter(|leaf| leaf.key == *key),
                Node::Branch(index) => {
                    node = self.branch(index).children[usize::from(bit(key, depth))];
                    depth += 1;
                }
            }
        }
    }

    /// The leaf at `key` in the tree under `root`, with the siblings proving it from the root down,
    /// or `None` when the tree holds no leaf at `key`.
    pub(crate) fn proven(&self, root: NodeRef, key: &B256) -> Option<(&Leaf, Vec<B256>)> {
        let (mut node, mut siblings) = (root, Vec::new());
        loop {
            match node.node() {
                Node::Empty => return None,
                Node::Leaf(index) => {
                    let leaf = self.leaf(index);
                    return (leaf.key == *key).then_some((leaf, siblings));
                }
                Node::Branch(index) => {
                    let side = usize::from(bit(key, siblings.len()));
                    let children = self.branch(index).children;
                    siblings.push(self.hash_of(children[1 - side]));
                    node = children[side];
                }
            }
        }
    }

    /// Writes `leaf` at `key` in the tree under `root`, or removes the leaf there for `None`, and
    /// returns the tree's new root. The caller gives up its hold on `root` for the one returned.
    /// Every branch on the path is copied if another version holds it, and marked stale when the
    /// leaf's hash changes (`rehash`). A removal collapses every branch left with one leaf under it.
    pub(crate) fn set(&mut self, root: NodeRef, key: &B256, leaf: Option<NewLeaf>, rehash: bool) -> NodeRef {
        self.set_at(root, 0, key, leaf, rehash)
    }

    /// `set` on the subtree at `depth` that `node` heads.
    fn set_at(&mut self, node: NodeRef, depth: usize, key: &B256, leaf: Option<NewLeaf>, rehash: bool) -> NodeRef {
        match node.node() {
            Node::Empty => leaf.map_or(NodeRef::EMPTY, |leaf| self.new_leaf(*key, leaf.hash, leaf.value)),
            Node::Leaf(index) if self.leaf(index).key == *key => {
                self.release(node);
                leaf.map_or(NodeRef::EMPTY, |leaf| self.new_leaf(*key, leaf.hash, leaf.value))
            }
            Node::Leaf(_) => match leaf {
                None => node,
                Some(leaf) => {
                    let fresh = self.new_leaf(*key, leaf.hash, leaf.value);
                    self.split(node, fresh, key, depth)
                }
            },
            Node::Branch(_) => {
                let branch = self.branch_to_change(node);
                let Node::Branch(index) = branch.node() else { unreachable!("a branch is copied to a branch") };
                let side = usize::from(bit(key, depth));
                let child = self.branch(index).children[side];
                let child = self.set_at(child, depth + 1, key, leaf, rehash);
                let changed = self.branch_mut(index);
                changed.children[side] = child;
                changed.stale |= rehash;
                let only = match changed.children.map(NodeRef::node) {
                    [Node::Empty, Node::Leaf(_)] => 1,
                    [Node::Leaf(_), Node::Empty] => 0,
                    // A branch held two or more leaves, and a write removes one at most.
                    [Node::Empty, Node::Empty] => unreachable!("a branch is left with no leaf"),
                    _ => return branch,
                };
                // One leaf is left under it: the leaf takes its place, and the branch is freed.
                let leaf = std::mem::replace(&mut changed.children[only], NodeRef::EMPTY);
                self.release(branch);
                leaf
            }
        }
    }

    /// The branches that part the leaves `old` and `new`, whose keys share their bits above `depth`,
    /// down to the depth where the keys differ. The caller moves its holds on both into them.
    fn split(&mut self, old: NodeRef, new: NodeRef, new_key: &B256, depth: usize) -> NodeRef {
        let old_key = self.leaf_key(old);
        let (old_bit, new_bit) = (bit(&old_key, depth), bit(new_key, depth));
        let children = match (old_bit, new_bit) {
            (false, false) => [self.split(old, new, new_key, depth + 1), NodeRef::EMPTY],
            (true, true) => [NodeRef::EMPTY, self.split(old, new, new_key, depth + 1)],
            (false, true) => [old, new],
            (true, false) => [new, old],
        };
        self.new_branch(children, EMPTY, true)
    }

    /// Hashes every stale branch at or below `from_depth` in the subtree at `depth` that `node`
    /// heads, bottom up, and returns how many it hashed. Stale branches above `from_depth` stay
    /// stale.
    pub(crate) fn rehash(&mut self, node: NodeRef, depth: usize, from_depth: usize) -> usize {
        let Node::Branch(index) = node.node() else { return 0 };
        let branch = *self.branch(index);
        if !branch.stale {
            return 0;
        }
        let below: usize = branch.children.into_iter().map(|child| self.rehash(child, depth + 1, from_depth)).sum();
        if depth < from_depth {
            return below;
        }
        let hash = node_hash(self.hash_of(branch.children[0]), self.hash_of(branch.children[1]));
        let hashed = self.branch_mut(index);
        (hashed.hash, hashed.stale) = (hash, false);
        below + 1
    }

    /// Builds the tree over `leaves`, sorted by key with no key twice, hashing every branch, and
    /// returns its root and how many branches it hashed.
    pub(crate) fn build(&mut self, leaves: Vec<(B256, NewLeaf)>) -> (NodeRef, usize) {
        let mut leaves: Vec<(B256, Option<NewLeaf>)> =
            leaves.into_iter().map(|(key, leaf)| (key, Some(leaf))).collect();
        let mut hashed = 0;
        let root = self.build_at(&mut leaves, 0, &mut hashed);
        (root, hashed)
    }

    /// `build` of the subtree at `depth` over `leaves`, which share their bits above it.
    fn build_at(&mut self, leaves: &mut [(B256, Option<NewLeaf>)], depth: usize, hashed: &mut usize) -> NodeRef {
        match leaves {
            [] => NodeRef::EMPTY,
            [(key, leaf)] => {
                let leaf = leaf.take().expect("each leaf is built once");
                self.new_leaf(*key, leaf.hash, leaf.value)
            }
            _ => {
                let split = leaves.partition_point(|(key, _)| !bit(key, depth));
                let (left, right) = leaves.split_at_mut(split);
                let left = self.build_at(left, depth + 1, hashed);
                let right = self.build_at(right, depth + 1, hashed);
                *hashed += 1;
                let hash = node_hash(self.hash_of(left), self.hash_of(right));
                self.new_branch([left, right], hash, false)
            }
        }
    }
}
