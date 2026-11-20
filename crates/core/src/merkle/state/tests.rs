use super::*;

use crate::command::Sequenced;

/// A tree taken back while an activation's discard waits, as an activation refused after setting it
/// leaves one, is dropped, as an engine that kept its tree would have dropped it, and the engine's
/// next root, over every leaf, is the root of one that never handed its tree out.
#[test]
fn a_tree_taken_back_with_a_discard_waiting_is_dropped_and_the_roots_stand() {
    let (mut kept, mut handed) = (Engine::new(), Engine::new());
    for sequenced in reference_commands().take(600) {
        kept.apply(&sequenced).unwrap();
        handed.apply(&sequenced).unwrap();
    }
    handed.state_root();
    let tree = handed.detach_tree().expect("a tree under V9");
    kept.discard_tree();
    handed.discard_tree();
    handed.attach_tree(tree);
    assert!(matches!(handed.tree, Tree::None), "the waiting discard drops the tree");
    assert_eq!(handed.state_root(), kept.state_root());
}

// ── Helpers ──

/// The commands of the `V84` reference log, which activates `V9` at seq 207.
fn reference_commands() -> impl Iterator<Item = Sequenced> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference/v84.jsonl");
    let log = std::fs::read_to_string(path).expect("the reference log");
    let commands: Vec<Sequenced> = log
        .lines()
        .map(|line| {
            let mut entry: serde_json::Value = serde_json::from_str(line).expect("a reference entry");
            entry.as_object_mut().expect("an object").remove("root");
            serde_json::from_value(entry).expect("a sequenced command")
        })
        .collect();
    commands.into_iter()
}
