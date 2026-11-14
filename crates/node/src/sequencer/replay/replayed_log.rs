//! A log read from disk and replayed, and what the log-head poster reads from it.

use anyhow::bail;
use tokio::sync::watch;

use crate::log::{LogWriter, Manifest, SegmentedLog};
use crate::sequencer::replay::Replayed;

/// A log read from disk and replayed, before anything is written to it.
pub struct ReplayedLog {
    pub(crate) log: LogSource,
    pub(crate) replayed: Replayed,
    pub(crate) snapshot_seq: Option<u64>,
    pub(crate) legacy_authorizations: crate::sequencer::legacy::Authorizations,
}

/// Where a recovered log's writer comes from: the log read from disk, or a follower's copy whose
/// writer is already open at the end its replica replayed.
pub(crate) enum LogSource {
    Read(SegmentedLog),
    Live(LogWriter),
}

impl From<SegmentedLog> for LogSource {
    fn from(log: SegmentedLog) -> Self {
        Self::Read(log)
    }
}

impl LogSource {
    /// A writer at the log's end, publishing the manifest on `manifest` (see
    /// `SegmentedLog::into_writer`); a follower's writer moves its manifest there as it stands.
    /// Error cases:
    /// - see `SegmentedLog::into_writer`
    pub(crate) fn into_writer(
        self,
        segment_commands: u64,
        manifest: watch::Sender<Manifest>,
    ) -> anyhow::Result<LogWriter> {
        match self {
            Self::Read(log) => log.into_writer(segment_commands, manifest),
            Self::Live(mut writer) => {
                writer.publish_on(manifest);
                Ok(writer)
            }
        }
    }

    /// The replay's commitments (see `SegmentedLog::manifest_view`); a follower's writer publishes
    /// its own.
    pub(crate) fn manifest_view(&self) -> Manifest {
        match self {
            Self::Read(log) => log.manifest_view(),
            Self::Live(writer) => writer.published(),
        }
    }

    /// See `SegmentedLog::attach_catalog`.
    /// Error cases:
    /// - as there, or the log is a follower's, which keeps no catalog
    pub(crate) fn attach_catalog(self, owner: crate::log::catalog_owner::CatalogOwner) -> anyhow::Result<Self> {
        match self {
            Self::Read(log) => Ok(Self::Read(log.attach_catalog(owner)?)),
            Self::Live(_) => bail!("a start from a follower's replica keeps no authenticated catalog"),
        }
    }

    /// See `SegmentedLog::bind_catalog_owner`.
    /// Error cases:
    /// - as there, or the log is a follower's, which keeps no catalog
    pub(crate) fn bind_catalog_owner(self, owner: crate::log::catalog_owner::CatalogOwner) -> anyhow::Result<Self> {
        match self {
            Self::Read(log) => Ok(Self::Read(log.bind_catalog_owner(owner)?)),
            Self::Live(_) => bail!("a start from a follower's replica keeps no authenticated catalog"),
        }
    }
}

impl ReplayedLog {
    /// A follower's copy, open at its end, and its replica's finished replay.
    pub(crate) fn live(writer: LogWriter, replayed: Replayed) -> Self {
        Self { log: LogSource::Live(writer), replayed, snapshot_seq: None, legacy_authorizations: Default::default() }
    }

    /// The cumulative command-prefix digest after the log's last command: the head commitment this
    /// node publishes on chain. A trailing checkpoint does not change it.
    pub fn head_link(&self) -> engine_core::eip712::B256 {
        engine_core::eip712::B256::from(self.replayed.command_prefix)
    }

    /// The replay-authenticated prefix ring: the last `crate::log::PrefixRing::CAPACITY`
    /// (seq, cumulative-prefix) pairs, the poster's adoption evidence for on-chain heads.
    pub(crate) fn prefix_ring(&self) -> crate::log::PrefixRing {
        self.replayed.prefix_ring.clone()
    }
}
