//! The state tree's nodes: branches and leaves in fixed-size chunks, so growth never moves a node,
//! each counting the parents and version roots that hold it. A node held once is changed in place; a
//! node held more than once is shared between versions and copied before it changes, so no version
//! sees another's changes. A node no one holds is freed, and its children released in turn.

use alloy_primitives::B256;

use crate::merkle::EMPTY;
use crate::merkle::tree::value::LeafValue;

/// Slots per chunk.
const CHUNK: usize = 1 << 16;
/// The tag bit of a leaf's reference.
const LEAF: u32 = 1 << 31;

/// An empty subtree, a branch or a leaf, by index into its table, in one `u32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NodeRef(u32);

/// What a `NodeRef` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Node {
    Empty,
    Branch(u32),
    Leaf(u32),
}

/// A subtree under which two or more leaves lie.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Branch {
    pub(crate) children: [NodeRef; 2],
    /// The subtree's hash, unless `stale`.
    pub(crate) hash: B256,
    /// Whether a leaf under it changed since `hash` was computed. Only a node the working version
    /// alone holds is stale: a version is hashed in full when it commits.
    pub(crate) stale: bool,
    held: u32,
}

/// A leaf, at the depth where its key parts from every other key.
#[derive(Debug)]
pub(crate) struct Leaf {
    pub(crate) key: B256,
    /// `leaf_hash(key, value hash)`.
    pub(crate) hash: B256,
    /// What proofs read from the leaf, for the kinds they read.
    pub(crate) value: Option<Box<LeafValue>>,
    held: u32,
}

/// One kind of node, in chunks that never reallocate.
#[derive(Debug)]
struct Table<T> {
    chunks: Vec<Vec<T>>,
    free: Vec<u32>,
    live: usize,
}

/// The nodes of every version of the tree.
#[derive(Debug, Default)]
pub(crate) struct Arena {
    branches: Table<Branch>,
    leaves: Table<Leaf>,
    /// Nodes no one holds, waiting to be freed by `collect`.
    dead: Vec<NodeRef>,
}

impl NodeRef {
    pub(crate) const EMPTY: Self = Self(u32::MAX);

    /// What the reference names.
    pub(crate) fn node(self) -> Node {
        match self.0 {
            u32::MAX => Node::Empty,
            r if r & LEAF != 0 => Node::Leaf(r & !LEAF),
            r => Node::Branch(r),
        }
    }

    /// Whether it names the empty subtree.
    pub(crate) fn is_empty(self) -> bool {
        self == Self::EMPTY
    }
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self { chunks: Vec::new(), free: Vec::new(), live: 0 }
    }
}

impl<T> Table<T> {
    /// Stores `item` in a free slot, or a new one, and returns its index.
    fn alloc(&mut self, item: T) -> u32 {
        self.live += 1;
        if let Some(index) = self.free.pop() {
            *self.get_mut(index) = item;
            return index;
        }
        if self.chunks.last().is_none_or(|chunk| chunk.len() == CHUNK) {
            self.chunks.push(Vec::with_capacity(CHUNK));
        }
        let index = (self.chunks.len() - 1) * CHUNK + self.chunks.last().map_or(0, Vec::len);
        // The leaf tag and `NodeRef::EMPTY` take the indexes from 2^31 up.
        let index =
            u32::try_from(index).ok().filter(|index| index & LEAF == 0).expect("fewer than 2^31 nodes of a kind");
        self.chunks.last_mut().expect("pushed above").push(item);
        index
    }

    /// The item at `index`.
    fn get(&self, index: u32) -> &T {
        &self.chunks[index as usize / CHUNK][index as usize % CHUNK]
    }

    /// The item at `index`, to change.
    fn get_mut(&mut self, index: u32) -> &mut T {
        &mut self.chunks[index as usize / CHUNK][index as usize % CHUNK]
    }

    /// Returns the slot at `index` to the free list.
    fn free(&mut self, index: u32) {
        self.live -= 1;
        self.free.push(index);
    }
}

impl Arena {
    /// A new branch over `children`, held once.
    pub(crate) fn new_branch(&mut self, children: [NodeRef; 2], hash: B256, stale: bool) -> NodeRef {
        NodeRef(self.branches.alloc(Branch { children, hash, stale, held: 1 }))
    }

    /// A new leaf, held once.
    pub(crate) fn new_leaf(&mut self, key: B256, hash: B256, value: Option<Box<LeafValue>>) -> NodeRef {
        NodeRef(self.leaves.alloc(Leaf { key, hash, value, held: 1 }) | LEAF)
    }

    /// The branch at `index`.
    pub(crate) fn branch(&self, index: u32) -> &Branch {
        self.branches.get(index)
    }

    /// The branch at `index`, to change: only one held once, which no other version shares.
    pub(crate) fn branch_mut(&mut self, index: u32) -> &mut Branch {
        self.branches.get_mut(index)
    }

    /// The leaf at `index`.
    pub(crate) fn leaf(&self, index: u32) -> &Leaf {
        self.leaves.get(index)
    }

    /// The hash of the subtree `node` heads, which must not be stale.
    pub(crate) fn hash_of(&self, node: NodeRef) -> B256 {
        match node.node() {
            Node::Empty => EMPTY,
            Node::Leaf(index) => self.leaf(index).hash,
            Node::Branch(index) => {
                let branch = self.branch(index);
                // Every stale branch is hashed before anything reads it: a version on commit, the
                // working version's branches below the band on each advance.
                assert!(!branch.stale, "a stale branch's hash is read");
                branch.hash
            }
        }
    }

    /// Notes one more holder of `node`.
    pub(crate) fn retain(&mut self, node: NodeRef) {
        match node.node() {
            Node::Empty => {}
            Node::Branch(index) => self.branches.get_mut(index).held += 1,
            Node::Leaf(index) => self.leaves.get_mut(index).held += 1,
        }
    }

    /// Notes one holder fewer of `node`, which `collect` frees once no one holds it.
    pub(crate) fn release(&mut self, node: NodeRef) {
        let held = match node.node() {
            Node::Empty => return,
            Node::Branch(index) => &mut self.branches.get_mut(index).held,
            Node::Leaf(index) => &mut self.leaves.get_mut(index).held,
        };
        // Every holder retains what it holds, so a release follows a retain.
        *held = held.checked_sub(1).expect("a released node is held");
        if *held == 0 {
            self.dead.push(node);
        }
    }

    /// Whether a node is held by more than one parent or version, and so must be copied to change.
    fn shared(&self, node: NodeRef) -> bool {
        match node.node() {
            Node::Empty => false,
            Node::Branch(index) => self.branch(index).held > 1,
            Node::Leaf(index) => self.leaf(index).held > 1,
        }
    }

    /// `branch` itself if its holder alone holds it, or else a copy for that holder, which releases
    /// the original: the branch a holder may change in place.
    pub(crate) fn branch_to_change(&mut self, branch: NodeRef) -> NodeRef {
        let Node::Branch(index) = branch.node() else { unreachable!("only a branch is copied to change") };
        if !self.shared(branch) {
            return branch;
        }
        let Branch { children, hash, stale, .. } = *self.branch(index);
        children.into_iter().for_each(|child| self.retain(child));
        self.release(branch);
        self.new_branch(children, hash, stale)
    }

    /// Frees up to `budget` nodes no one holds, releasing their children, and returns how many it
    /// freed. Freeing is exact: a node is freed once, when its last holder lets it go.
    pub(crate) fn collect(&mut self, budget: usize) -> usize {
        let mut freed = 0;
        while freed < budget {
            let Some(node) = self.dead.pop() else { break };
            match node.node() {
                Node::Empty => unreachable!("an empty subtree is never held"),
                Node::Branch(index) => {
                    let children = self.branch(index).children;
                    self.branches.free(index);
                    children.into_iter().for_each(|child| self.release(child));
                }
                Node::Leaf(index) => {
                    self.leaves.get_mut(index).value = None;
                    self.leaves.free(index);
                }
            }
            freed += 1;
        }
        freed
    }

    /// Nodes no one holds that `collect` has yet to free.
    pub(crate) fn dead(&self) -> usize {
        self.dead.len()
    }

    /// The branches and leaves in use, freed slots and dead nodes aside.
    pub(crate) fn live(&self) -> (usize, usize) {
        (self.branches.live, self.leaves.live)
    }

    /// The key of the leaf `node` names.
    pub(crate) fn leaf_key(&self, node: NodeRef) -> B256 {
        let Node::Leaf(index) = node.node() else { unreachable!("a leaf is split") };
        self.leaf(index).key
    }
}
