//! Writes a protocol reference log: a session through every command and main path, with each command's state
//! root. Run once per protocol version and commit the output; it refuses to overwrite. Usage:
//! `cargo run -p engine-core --example reference_log -- crates/core/tests/reference/v<N>.jsonl`

#[path = "../../tests/common/mod.rs"]
mod common;
mod helpers;
mod session;
mod stress;
mod v10;
mod v11;
mod v12;
mod v14;
mod v15;
mod v16;
mod v17;
mod v18;
mod v19;
mod v20;
mod v21;
mod v22;
mod v23;
mod v24;
mod v25;
mod v26;
mod v27;
mod v28;
mod v29;
mod v30;
mod v31;
mod v32;
mod v33;
mod v34;
mod v35;
mod v37;
mod v38;
mod v39;
mod v40;
mod v41;
mod v42;
mod v43;
mod v44;
use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::anyhow;
use common::reference::{ReferenceEntry, root_hex};
use engine_core::Engine;
use engine_core::types::MarketId;
use helpers::{event_kind, write_entries};

pub(crate) const SOL: MarketId = 10;
pub(crate) const GOLD: MarketId = 11;
pub(crate) const OIL: MarketId = 12;

/// Writes the session to the given path and prints the event counts.
/// Fails if the path is missing, already exists, or cannot be written.
fn main() -> anyhow::Result<()> {
    let path: PathBuf =
        std::env::args_os().nth(1).ok_or_else(|| anyhow!("usage: reference_log <output.jsonl>"))?.into();
    let mut engine = Engine::new();
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    let mut entries = Vec::new();
    for sequenced in session::session() {
        for event in engine.apply(&sequenced)? {
            *tally.entry(event_kind(&event)).or_default() += 1;
        }
        entries.push(ReferenceEntry { sequenced, root: root_hex(engine.state_root()) });
    }
    write_entries(&path, &entries)?;
    println!("wrote {} commands to {}", entries.len(), path.display());
    tally.iter().for_each(|(kind, count)| println!("{count:>5} {kind}"));
    Ok(())
}
