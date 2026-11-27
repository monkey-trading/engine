//! What the scenario puts in a release node's working directory, as its operator does: its config, a
//! copy of the whole directory, and the admission of its enclave's standby key.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::secrets::tests::operator_key;
use crate::sequencer::withdrawal_scenario::admission::admit;
use crate::sequencer::withdrawal_scenario::keys::ReleaseNode;
use crate::sequencer::withdrawal_scenario::node::write_config;
use crate::sequencer::withdrawal_scenario::rpc::Chain;
use crate::sequencer::withdrawal_scenario::services::Services;

/// Writes a release's config to `path`, relative to the node's working directory, with ephemeral
/// ports and its alert outbox in `outbox`, and returns `path`.
/// Error cases:
/// - the config cannot be written
pub(super) fn release_config(path: &Path, outbox: &str, chain: &Chain, services: &Services) -> anyhow::Result<PathBuf> {
    write_config(path, Path::new(""), 0, 0, chain, services, |cfg| {
        cfg["alerts"]["outbox"]["dir"] = outbox.into();
        cfg["tee"]["use_published_mock_key"] = false.into();
        cfg["tee"]["sealing"] = json!({
            "dir": "keys",
            "handover": { "listen": "127.0.0.1:0" },
            "secrets": { "listen": "127.0.0.1:0" },
        });
        cfg["alerts"]["warning"] = json!({ "url_secret": "alerts_warning_url" });
        Ok(())
    })?;
    Ok(path.to_owned())
}

impl ReleaseNode {
    /// Admits this release's enclave as the operator does with `engine admin sign-admission`: the
    /// operator key the log holds signs an admission of the standby key the enclave sealed for this
    /// release, which a take seals first, into its sealing directory.
    /// Error cases:
    /// - see `admission::admit`
    pub(super) fn admit(&self) -> anyhow::Result<()> {
        admit(&self.data.join("keys"), self.release, &operator_key(), self.vault)
    }
}

/// Copies the directory tree `from` to `to`.
/// Error cases:
/// - a read or write fails
pub(super) fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
