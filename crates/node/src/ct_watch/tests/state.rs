use std::path::PathBuf;

use crate::ct_watch::state::{Finding, LogProgress, State, StateFile, suffixed};
use crate::test_support::scratch_dir;

fn sample() -> State {
    let mut state = State::default();
    state.attested.insert("aa".repeat(32), 5);
    state.logs.insert("log".into(), LogProgress { name: "log".into(), next_index: Some(9), caught_up_ms: 7 });
    state.findings.push(Finding {
        log: "log".into(),
        index: 8,
        issuer: "CN=Issuer".into(),
        key_sha256: "bb".repeat(32),
        not_after_ms: 99,
        decide_at_ms: 11,
        alerted_at_ms: None,
        resolving: false,
    });
    state
}

fn path(name: &str) -> PathBuf {
    let dir = scratch_dir(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("watch").join("state.json")
}

#[test]
fn state_saved_is_state_loaded_and_a_missing_file_starts_empty() {
    let path = path("ct-state-roundtrip");
    let (file, state) = StateFile::open(&path).unwrap();
    assert_eq!(state, State::default());
    file.save(&sample()).unwrap();
    drop(file);
    let (_, loaded) = StateFile::open(&path).unwrap();
    assert_eq!(loaded, sample());
    assert!(!suffixed(&path, ".tmp").exists(), "the temporary file is renamed away");
}

#[test]
fn a_second_watcher_on_the_same_state_is_refused_until_the_first_stops() {
    let path = path("ct-state-lock");
    let (first, _) = StateFile::open(&path).unwrap();
    let err = StateFile::open(&path).err().unwrap();
    assert!(format!("{err:#}").contains("another watcher"), "{err:#}");
    drop(first);
    assert!(StateFile::open(&path).is_ok());
}

#[test]
fn an_unreadable_state_is_never_replaced_by_an_empty_one() {
    let path = path("ct-state-corrupt");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    for bad in ["not json", r#"{"attested":{},"logs":{},"findings":[],"extra":1}"#] {
        std::fs::write(&path, bad).unwrap();
        assert!(StateFile::open(&path).is_err(), "{bad}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
    }
}

#[test]
fn a_state_file_named_like_its_own_temporary_or_lock_file_is_still_saved_atomically() {
    for name in ["watch.tmp", "watch.lock", "watch"] {
        let path = path("ct-state-names").with_file_name(name);
        assert_ne!(suffixed(&path, ".tmp"), path, "{name}: the temporary file is the state file");
        assert_ne!(suffixed(&path, ".lock"), path, "{name}: the lock file is the state file");
        let (file, _) = StateFile::open(&path).unwrap();
        file.save(&sample()).unwrap();
        // A crash between the temporary write and the rename leaves the live file whole.
        let live = std::fs::read(&path).unwrap();
        let mut temp = path.clone().into_os_string();
        temp.push(".tmp");
        std::fs::write(&temp, b"half").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), live, "{name}");
        drop(file);
        let (_, loaded) = StateFile::open(&path).unwrap();
        assert_eq!(loaded, sample(), "{name}");
    }
}
