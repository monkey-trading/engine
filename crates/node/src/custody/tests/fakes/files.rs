//! Temporary directories, configuration files, and the stored deposit cursor for the custody fixtures.

use std::path::{Path, PathBuf};

use crate::custody::cursor::Cursor;

/// A `[custody.evm]` section with its cursor in `dir`.
pub(in crate::custody::tests) fn evm_toml(dir: &Path, max_block_range: u64) -> String {
    format!(
        r#"
        providers = [
          {{ name = "a", rpc_url = "http://127.0.0.1:1" }},
          {{ name = "b", rpc_url = "http://127.0.0.1:2" }},
          {{ name = "c", rpc_url = "http://127.0.0.1:3" }},
        ]
        usdc = "0xc0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0"
        finality_tag = "safe"
        withdrawal_finality_tag = "finalized"
        confirmations = 0
        start_block = 0
        max_block_range = {max_block_range}
        poll_interval = "10ms"
        rpc_timeout = "1s"
        stall_critical_after = "1s"
        finality_stalled_after = "1s"
        cursor_path = "{}"
        "#,
        dir.join("cursor.json").display()
    )
}

/// A fresh, empty directory for one test.
pub(in crate::custody::tests) fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("engine-custody-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The cursor stored in `dir`, read without the checks `Cursor::load` makes.
pub(in crate::custody::tests) fn stored_cursor(dir: &Path) -> Cursor {
    serde_json::from_str(&std::fs::read_to_string(dir.join("cursor.json")).unwrap()).unwrap()
}
