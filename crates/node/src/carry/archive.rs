//! A member's check of its config's `[genesis.carry]` against the archived log's genesis, for when it holds no copy:
//! while rotation is off it registers its standby key before it follows, so it reads the operator's archive.

use anyhow::Context;
use engine_core::carry::{CarryPart, CarrySource};
use engine_core::command::{AdminCommand, Command};
use tracing::{error, warn};

use crate::config::NodeConfig;
use crate::log::LogEntry;
use crate::standby::Followed;
use crate::standby::archive::{archive_client, fetch_archive_bytes, fetch_manifest};

/// Refuses the log `operator` archives when its genesis carried from another source than the config's
/// `[genesis.carry]`, from any when the config names none, or from none when it names one
/// (`boot::check_carried_source`), paging Critical `carry_refused`; a failed read is a warning. An archive that does
/// not yet hold the whole genesis passes, as a fresh log's copy does.
/// Error cases:
/// - the archive cannot be read, or does not parse
/// - the archived genesis carried from another source than the config names
pub(crate) async fn check_archived(cfg: &NodeConfig, operator: &Followed) -> anyhow::Result<()> {
    let archive = archive_client(operator.danger_accept_invalid_certs)?;
    let source = archived_source(&archive, &operator.origins())
        .await
        .inspect_err(|e| warn!("reading the archived genesis to check [genesis.carry] failed: {e:#}"))?;
    let (carried, open) = source.map_or((None, true), |carried| (carried, false));
    super::boot::check_carried_source(carried, super::boot::configured(cfg), open)
        .inspect_err(|e| error!(alert = "carry_refused", "refusing to register the standby key: {e:#}"))
}

/// The source the genesis of the log `origins` archive carried from, read from its segments in order up to the
/// house capital credit that ends genesis: `Some(None)` for a genesis that carries nothing, `None` while the
/// archive holds no whole genesis.
/// Error cases:
/// - the manifest or a segment cannot be fetched, differs from its listing, or holds a line that does not parse
pub(crate) async fn archived_source(
    archive: &reqwest::Client,
    origins: &[String],
) -> anyhow::Result<Option<Option<CarrySource>>> {
    for listed in fetch_manifest(archive, origins).await? {
        let bytes = fetch_archive_bytes(archive, origins, &listed.file, Some(&listed)).await?;
        for line in bytes.split(|b| *b == b'\n').filter(|line| !line.trim_ascii().is_empty()) {
            let entry: LogEntry = serde_json::from_slice(line).with_context(|| format!("parsing {}", listed.file))?;
            let LogEntry::Command(command) = entry else { continue };
            match command.sequenced.cmd {
                Command::Admin(AdminCommand::CarryState(CarryPart::Begin { source, .. })) => {
                    return Ok(Some(Some(source)));
                }
                cmd if crate::sequencer::genesis::is_genesis_credit(&cmd) => return Ok(Some(None)),
                _ => {}
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
