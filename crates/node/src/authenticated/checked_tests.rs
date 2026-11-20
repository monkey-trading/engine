//! Nodes one operation's readers share once checked, and reads of a root over one session.

use anyhow::Result;
use jmt::storage::Node;

use crate::authenticated::reader::{Checked, Reader};
use crate::authenticated::tests::{fresh, put, raw};
use crate::authenticated::{Root, codec, key_hash};

/// A node one reader of an operation checked is taken by the operation's other readers without being
/// read again, while a new operation reads and checks it afresh: nodes the host damages after the
/// first read leave the operation's later reads as they were and refuse the next operation's.
#[test]
fn an_operations_readers_share_the_nodes_it_checked_and_the_next_operation_checks_afresh() -> Result<()> {
    let (dir, store) = fresh()?;
    let namespace = [21; 32];
    let root = store.update(&Root::empty(namespace), vec![put(b"a", b"A"), put(b"b", b"B"), put(b"c", b"C")])?;
    let conn = raw(&dir)?;
    let checked = Checked::default();
    let key = key_hash(&namespace, b"a")?;
    let first = Reader::new(&conn, &root, false, &checked)?.read(key)?;
    conn.execute("UPDATE nodes SET data = ?1", [br#""Null""#.as_slice()])?;
    assert_eq!(Reader::new(&conn, &root, false, &checked)?.read(key)?, first, "the operation's nodes are kept");
    assert!(Reader::new(&conn, &root, false, &Checked::default())?.read(key).is_err(), "a new operation reads afresh");
    assert!(store.read(&root, b"a").is_err(), "a store read is a new operation");
    Ok(())
}

/// A node an operation recorded is taken only where a checked parent names the hash it was checked to
/// have: one recorded under another hash is read and checked afresh, and one forged under the parent's
/// hash still cannot pass the root's proof.
#[test]
fn a_recorded_node_is_taken_only_under_the_hash_it_was_checked_to_have() -> Result<()> {
    let (dir, store) = fresh()?;
    let namespace = [22; 32];
    let root = store.update(&Root::empty(namespace), vec![put(b"a", b"A"), put(b"b", b"B")])?;
    let conn = raw(&dir)?;
    let (key, root_key) = (key_hash(&namespace, b"a")?, codec::root_key(root.version.expect("a version"))?);
    let other = Checked::default();
    other.borrow_mut().insert(root_key.clone(), (Node::Null, [7; 32]));
    assert_eq!(Reader::new(&conn, &root, false, &other)?.read(key)?, Some(b"A".to_vec()), "another hash is not taken");
    let forged = Checked::default();
    forged.borrow_mut().insert(root_key, (Node::Null, root.hash));
    assert!(
        Reader::new(&conn, &root, false, &forged).and_then(|reader| reader.read(key)).is_err(),
        "a forged node proves nothing"
    );
    Ok(())
}

/// A session reads each key as a lease does, and proves every value it reads: a value the host damages
/// is refused by a session's read of it. The session holds the database's shared lock until dropped,
/// so the damage is done between sessions.
#[test]
fn a_session_reads_as_a_lease_does_and_proves_every_value() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([23; 32]), vec![put(b"a", b"A"), put(b"b", b"B")])?;
    let lease = store.lease(&root)?;
    let mut session = lease.session();
    for key in [b"a".as_slice(), b"b", b"missing"] {
        assert_eq!(session.read(key)?, lease.read(key)?, "{key:?}");
    }
    drop(session);
    raw(&dir)?.execute("UPDATE value_rows SET data = ?1", [b"forged".as_slice()])?;
    assert!(lease.session().read(b"a").is_err(), "a damaged value is refused");
    Ok(())
}
