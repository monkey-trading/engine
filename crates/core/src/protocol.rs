//! Protocol versions. A version is a set of rules. A log moves to a newer version with
//! `AdminCommand::ActivateProtocol`, so each command applies under the rules active at its seq, and
//! a log that never activates one replays under `V2`, the rules every log starts under. Versions
//! are named after their reference logs in `crates/core/tests/reference/`; `V2`'s rules took effect
//! from genesis before versions could activate, so `v1.jsonl` and `v2.jsonl` both replay under it.
//!
//! Adding a version:
//! 1. Add a variant after the last one, in `protocol/versions.rs`, and point
//!    [`ProtocolVersion::LATEST`], in `protocol/support.rs`, at it; the node activates `LATEST` on its
//!    next boot, at the next seq of each log. Activation is one-way: a build that predates the version
//!    cannot read a log that activated it, so a rollback needs a fresh log.
//! 2. Gate each changed rule on `self.protocol_version >= ProtocolVersion::Vn`, and leave the
//!    earlier path as it is.
//! 3. Keep the roots of earlier versions. Before `V9` the root hashes the engine's encoding, so
//!    skip new engine state from serialization while it holds its default (`skip_serializing_if`).
//!    From `V9` the root hashes the state tree's leaves (`merkle`): the leaf encodings name every
//!    field they cover and refuse to compile until new state has a leaf, so put it in one and
//!    encode it only while it differs from its default, or under the new version. A request type
//!    that changes gets new typed structs under a new `eip712::DOMAIN_VERSION`, chosen by protocol
//!    version, and the earlier ones stay so earlier signatures still verify. The operator signs
//!    an admin command as its JSON, so a field added to anything an admin command carries is
//!    skipped while it holds its default too, or logged signatures stop verifying.
//! 4. Write and commit the version's reference log, as `crates/core/README.md` ("Adding a protocol
//!    version") describes. Every earlier reference log must still replay to its roots.

mod support;
mod versions;
pub use versions::ProtocolVersion;
