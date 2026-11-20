//! Nodes one operation's readers share once checked, and reads of a root over one session.

use anyhow::Result;
use jmt::SimpleHasher;
use jmt::storage::Node;

use crate::authenticated::blobs::Blob;
use crate::authenticated::reader::{Checked, Reader};
use crate::authenticated::tests::{fresh, put, raw};
use crate::authenticated::{Hasher, Root, codec, key_hash};

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
    other.nodes().borrow_mut().insert(root_key.clone(), (Node::Null, [7; 32]));
    assert_eq!(Reader::new(&conn, &root, false, &other)?.read(key)?, Some(b"A".to_vec()), "another hash is not taken");
    let forged = Checked::default();
    forged.nodes().borrow_mut().insert(root_key, (Node::Null, root.hash));
    assert!(
        Reader::new(&conn, &root, false, &forged).and_then(|reader| reader.read(key)).is_err(),
        "a forged node proves nothing"
    );
    Ok(())
}

/// A session reads each key as a lease does, and proves every value it reads, a later read's as its
/// first's: a value the host damaged is refused by the read that meets it, after reads that succeeded.
/// The session holds the database's shared lock until dropped, so the damage is done before it opens.
#[test]
fn a_session_reads_as_a_lease_does_and_proves_every_value() -> Result<()> {
    let (dir, store) = fresh()?;
    let namespace = [23; 32];
    let root = store.update(&Root::empty(namespace), vec![put(b"a", b"A"), put(b"b", b"B")])?;
    let lease = store.lease(&root)?;
    let mut session = lease.session();
    for key in [b"a".as_slice(), b"b", b"missing"] {
        assert_eq!(session.read(key)?, lease.read(key)?, "{key:?}");
    }
    drop(session);
    // A well-formed descriptor of the empty value, which only the value's proof refuses.
    let forged = serde_json::to_vec(&Blob { length: 0, digest: <Hasher as SimpleHasher>::new().finalize() })?;
    let damaged = key_hash(&namespace, b"b")?;
    raw(&dir)?.execute("UPDATE value_rows SET data = ?1 WHERE key = ?2", (forged, &damaged.0[..]))?;
    let mut session = lease.session();
    assert_eq!(session.read(b"a")?, Some(b"A".to_vec()), "an undamaged value reads");
    assert!(session.read(b"b").is_err(), "the session's later read refuses the damaged value");
    Ok(())
}

/// A session checks the schema at its first read and refuses that read on a schema the host changed; a
/// session whose first read was refused checks it afresh at its next.
#[test]
fn a_session_checks_the_schema_at_its_first_read() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([25; 32]), vec![put(b"a", b"A")])?;
    let lease = store.lease(&root)?;
    let conn = raw(&dir)?;
    conn.execute_batch("CREATE TRIGGER injected AFTER INSERT ON value_rows BEGIN DELETE FROM nodes; END;")?;
    let mut session = lease.session();
    assert!(session.read(b"a").is_err(), "a changed schema is refused at the first read");
    assert!(session.read(b"a").is_err(), "a refused first read leaves the schema to check at the next");
    conn.execute_batch("DROP TRIGGER injected")?;
    assert_eq!(session.read(b"a")?, Some(b"A".to_vec()), "the next read checks the schema afresh");
    Ok(())
}

/// An operation keeps at most its limit of nodes checked, however many it reads: past it it forgets
/// what it recorded and checks nodes afresh, and every read still gives its value.
#[test]
fn an_operation_keeps_a_bounded_number_of_checked_nodes() -> Result<()> {
    const LIMIT: usize = 64;
    let (dir, store) = fresh()?;
    let namespace = [24; 32];
    let keys: Vec<Vec<u8>> = (0..200u32).map(|n| n.to_be_bytes().to_vec()).collect();
    let root = store.update(&Root::empty(namespace), keys.iter().map(|key| put(key, key)).collect())?;
    let conn = raw(&dir)?;
    let checked = Checked::with_limit(LIMIT);
    let (mut kept, mut forgotten) = (0, false);
    for key in &keys {
        let read = Reader::new(&conn, &root, false, &checked)?.read(key_hash(&namespace, key)?)?;
        assert_eq!(read.as_deref(), Some(key.as_slice()), "{key:?}");
        let now = checked.nodes().borrow().len();
        assert!(now <= LIMIT, "the operation keeps {now}");
        forgotten |= now < kept;
        kept = now;
    }
    assert!(forgotten, "the reads never passed the limit");
    Ok(())
}
