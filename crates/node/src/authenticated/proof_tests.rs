//! A read walks and checks only the nodes on its key's path, and still proves every value and absence.

use anyhow::Result;

use crate::authenticated::reader::{Checked, Reader};
use crate::authenticated::tests::{fresh, put, raw};
use crate::authenticated::{Root, key_hash};

/// Reads of present and absent keys through trees of every depth the fixture reaches give each value, and
/// absence.
#[test]
fn reads_prove_every_value_and_absence() -> Result<()> {
    let (_dir, store) = fresh()?;
    let mut root = Root::empty([41; 32]);
    for round in 0..3u32 {
        let changes = (round * 400..(round + 1) * 400).map(|n| put(&n.to_be_bytes(), &n.to_le_bytes())).collect();
        root = store.update(&root, changes)?;
    }
    for n in 0..1_400u32 {
        let expected = (n < 1_200).then(|| n.to_le_bytes().to_vec());
        assert_eq!(store.read(&root, &n.to_be_bytes())?, expected, "key {n}");
    }
    Ok(())
}

/// A read of one key checks one node a level of its path, with none of the siblings beside it, and the
/// same read again, of a value or of an absence, checks nothing more.
#[test]
fn a_read_checks_only_the_nodes_on_its_path() -> Result<()> {
    let (dir, store) = fresh()?;
    let namespace = [42; 32];
    let changes = (0..1_200u32).map(|n| put(&n.to_be_bytes(), &n.to_le_bytes())).collect();
    let root = store.update(&Root::empty(namespace), changes)?;
    let conn = raw(&dir)?;
    let checked = Checked::default();
    let reader = Reader::new(&conn, &root, true, &checked)?;
    let present = key_hash(&namespace, &7u32.to_be_bytes())?;
    assert!(reader.read(present)?.is_some());
    let path = checked.nodes().borrow().len();
    assert!((2..=6).contains(&path), "{path} nodes checked for one key in 1,200");
    assert!(reader.read(present)?.is_some());
    assert_eq!(checked.nodes().borrow().len(), path, "a second read of the key checks nothing more");
    assert_eq!(reader.read(key_hash(&namespace, b"absent")?)?, None);
    Ok(())
}
