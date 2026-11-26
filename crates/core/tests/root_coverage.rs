//! The state root binds the whole state. At the states of the latest version's reference log, every leaf of the
//! engine's encoding is changed in turn, and the engine decoded from the changed encoding, as a start decodes a
//! snapshot's, must have another root wherever it encodes otherwise. A leaf that no single change leaves decodable,
//! such as an internally tagged enum's tag, goes unchecked; a change the decoding drops leaves no other state.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;

use common::reference::{ReferenceEntry, root_hex};
use engine_core::Engine;
use engine_core::protocol::ProtocolVersion;
use serde_json::Value;

/// The shapes of the record fields the root binds from `V91`, which the states must hold and the check reach.
const RECORDS: [&str; 4] = [
    "pending_deposits/*/chain_ref",
    "held_deposits/*/deposit/chain_ref",
    "held_deposits/*/deposit/payer",
    "held_deposits/*/reason",
];

/// What changing one leaf showed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// A change decoded to another state, with another root.
    Bound,
    /// A change decoded to another state with the same root.
    Unbound,
    /// Every change that decoded encoded as the original: the decoding drops it.
    Dropped,
    /// No change decoded.
    Undecodable,
}

#[test]
fn every_leaf_of_the_encoding_moves_the_root() {
    let mut reached: BTreeMap<String, Verdict> = BTreeMap::new();
    let mut unbound = Vec::new();
    for (original, logged) in latest_states() {
        let (root, again) = decoded(&original).expect("the engine's encoding decodes");
        assert_eq!((again == original, root_hex(root)), (true, logged), "the encoding decodes to itself and its root");
        let mut leaves = Vec::new();
        collect_leaves(&original, &mut Vec::new(), &mut Vec::new(), &mut leaves);
        for ((path, shape, _), verdict) in leaves.iter().zip(verdicts(&original, root, &leaves)) {
            if verdict == Verdict::Unbound {
                unbound.push(path.join("/"));
            }
            let seen = reached.entry(shape.clone()).or_insert(verdict);
            if verdict == Verdict::Bound || *seen == Verdict::Undecodable {
                *seen = verdict;
            }
        }
    }
    assert!(unbound.is_empty(), "the root does not bind {unbound:#?}");
    for shape in RECORDS {
        assert_eq!(reached.get(shape), Some(&Verdict::Bound), "{shape}");
    }
    let unchecked: Vec<&String> =
        reached.iter().filter(|(_, verdict)| **verdict == Verdict::Undecodable).map(|(shape, _)| shape).collect();
    eprintln!("{} shapes checked; no single change decodes for {unchecked:?}", reached.len());
}

/// The engine's encoding and the root the log records after the first command of the latest version's reference log
/// under that version, and after its last.
fn latest_states() -> Vec<(Value, String)> {
    let name = format!("{:?}.jsonl", ProtocolVersion::LATEST).to_lowercase();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/reference").join(&name);
    let text = fs::read_to_string(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
    let entries: Vec<ReferenceEntry> =
        text.lines().map(|line| serde_json::from_str(line).expect("a reference entry")).collect();
    let mut engine = Engine::new();
    let mut states = Vec::new();
    for (n, entry) in entries.iter().enumerate() {
        engine.apply(&entry.sequenced).expect("the reference log replays");
        let first = states.is_empty() && engine.protocol_version() == ProtocolVersion::LATEST;
        if first || n + 1 == entries.len() {
            states.push((serde_json::to_value(&engine).expect("the engine encodes"), entry.root.clone()));
        }
    }
    assert_eq!(states.len(), 2, "{name} activates the latest version before its last command");
    states
}

/// The verdict of changing each of `leaves` of `original`, whose engine's root is `root`, the leaves split across the
/// machine's cores. The values of the encoding's pool are tried for a shape until no value of it changes a leaf of the
/// shape to another state.
fn verdicts(original: &Value, root: [u8; 32], leaves: &[Leaf]) -> Vec<Verdict> {
    let pool = replacements(leaves);
    let threads = std::thread::available_parallelism().map_or(1, |cores| cores.get());
    std::thread::scope(|scope| {
        let parts: Vec<_> = leaves
            .chunks(leaves.len().div_ceil(threads).max(1))
            .map(|part| {
                let pool = &pool;
                scope.spawn(move || {
                    let mut fruitless: BTreeSet<&str> = BTreeSet::new();
                    let mut verdicts = Vec::with_capacity(part.len());
                    for (path, shape, leaf) in part {
                        let tried = if fruitless.contains(shape.as_str()) { &[][..] } else { &pool[..] };
                        let verdict = change(original, path, leaf, root, tried);
                        if matches!(verdict, Verdict::Dropped | Verdict::Undecodable) {
                            fruitless.insert(shape.as_str());
                        }
                        verdicts.push(verdict);
                    }
                    verdicts
                })
            })
            .collect();
        parts.into_iter().flat_map(|part| part.join().expect("a checking thread runs to its end")).collect()
    })
}

/// The verdict of changing `leaf`, at `path` of `original`, whose engine's root is `root`: the first change that
/// decodes to another encoding decides it.
fn change(original: &Value, path: &[String], leaf: &Value, root: [u8; 32], pool: &[Value]) -> Verdict {
    let mut verdict = Verdict::Undecodable;
    for replacement in changes(leaf).into_iter().chain(pool.iter().filter(|value| *value != leaf).cloned()) {
        let mut changed = original.clone();
        *at(&mut changed, path) = replacement;
        match decoded(&changed) {
            None => {}
            Some((_, again)) if again == *original => verdict = Verdict::Dropped,
            Some((moved, _)) => return if moved == root { Verdict::Unbound } else { Verdict::Bound },
        }
    }
    verdict
}

/// The root and encoding of the engine decoded from `value` as a start decodes a snapshot's, or `None` when it does
/// not decode, or its indexes cannot be restored.
fn decoded(value: &Value) -> Option<([u8; 32], Value)> {
    panic::catch_unwind(AssertUnwindSafe(|| {
        let mut engine: Engine = serde_json::from_value(value.clone()).ok()?;
        engine.rebuild_resting();
        engine.restore_indexes();
        let again = serde_json::to_value(&engine).ok()?;
        Some((engine.state_root(), again))
    }))
    .ok()
    .flatten()
}

/// A leaf of an encoding: its path, its shape, and its value.
type Leaf = (Vec<String>, String, Value);

/// Collects each leaf of `value` into `leaves`, its shape the path with each list index as `#` and each map key that
/// names an entry, not a field, as `*`.
fn collect_leaves(value: &Value, path: &mut Vec<String>, shape: &mut Vec<String>, leaves: &mut Vec<Leaf>) {
    let mut descend = |step: String, part: String, child: &Value| {
        path.push(step);
        shape.push(part);
        collect_leaves(child, path, shape, leaves);
        path.pop();
        shape.pop();
    };
    match value {
        Value::Object(map) => map.iter().for_each(|(key, child)| {
            let field = key.starts_with(|c: char| c.is_ascii_lowercase())
                && key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            descend(key.clone(), if field { key.clone() } else { "*".into() }, child)
        }),
        Value::Array(items) => {
            items.iter().enumerate().for_each(|(index, child)| descend(index.to_string(), "#".into(), child))
        }
        leaf => leaves.push((path.clone(), shape.join("/"), leaf.clone())),
    }
}

/// The value at `path` in `value`.
fn at<'a>(value: &'a mut Value, path: &[String]) -> &'a mut Value {
    path.iter().fold(value, |value, step| match value {
        Value::Object(map) => map.get_mut(step).expect("a collected path"),
        Value::Array(items) => &mut items[step.parse::<usize>().expect("a list index")],
        _ => unreachable!("a collected path ends at a leaf"),
    })
}

/// Changes of `leaf` to try first: a flipped flag, a number one away, a string's last digit or letter changed, and a
/// null as a number, a string or a flag.
fn changes(leaf: &Value) -> Vec<Value> {
    match leaf {
        Value::Bool(flag) => vec![Value::Bool(!flag)],
        Value::Number(number) => match (number.as_i64(), number.as_u64()) {
            (Some(n), _) => vec![Value::from(n + 1), Value::from(n - 1)],
            (None, Some(n)) => vec![Value::from(n - 1)],
            _ => Vec::new(),
        },
        Value::String(text) => {
            let mut changed = Vec::new();
            if let Some(last) = text.chars().last() {
                let swapped = match last {
                    '0' => '1',
                    '1'..='9' => '0',
                    'a' => 'b',
                    'b'..='f' => 'a',
                    _ => 'x',
                };
                changed.push(Value::String(format!("{}{swapped}", &text[..text.len() - last.len_utf8()])));
            }
            if let Ok(n) = text.parse::<i64>() {
                changed.push(Value::String((n + 1).to_string()));
            }
            changed
        }
        Value::Null => vec![Value::from(1), Value::String("1".into()), Value::Bool(true)],
        _ => Vec::new(),
    }
}

/// Values among `leaves` to try where no change of a leaf decodes: each name of a variant, as a lower-case
/// identifier, each address, and null.
fn replacements(leaves: &[Leaf]) -> Vec<Value> {
    let mut pool: Vec<Value> = Vec::new();
    for (_, _, value) in leaves {
        let Value::String(text) = value else { continue };
        let identifier = text.len() < 40 && text.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        let address = text.len() == 42 && text.starts_with("0x");
        if (identifier || address) && !pool.contains(value) {
            pool.push(value.clone());
        }
    }
    pool.push(Value::Null);
    pool
}
