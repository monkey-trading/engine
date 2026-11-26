//! Where a log's daily snapshots stand in the archive's bucket: each the file as its node published it, at
//! `snapshots/<chain id>/<vault>/<file>`, under the log's custody chain and vault as its ranges are, so a new log
//! never meets an earlier log's objects. A node whose host no longer holds a snapshot it lists redirects it (`308`)
//! to its object, which a reader takes only as the bytes the listing names.

use anyhow::{Context, ensure};
use engine_core::types::AccountId;
use reqwest::Url;

use crate::config::NodeConfig;
use crate::daily_snapshots::offset_named;

/// The public URL of a log's daily snapshots in the archive's bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotBucket {
    /// The URL of the log's snapshot prefix, ending in `/`: each snapshot's object is its file name joined to it.
    base: Url,
}

/// The key prefix of a log's daily snapshots: its custody chain and vault, which no other log shares.
pub(crate) fn snapshots_prefix(chain_id: u64, vault: AccountId) -> String {
    format!("snapshots/{chain_id}/{vault}/")
}

impl SnapshotBucket {
    /// The bucket `cfg`'s `[archive]` keeps the node's own log's daily snapshots in, at its public origin. None
    /// without the section.
    /// Error cases:
    /// - the custody vault does not parse, or the prefix does not join the origin
    pub(crate) fn for_config(cfg: &NodeConfig) -> anyhow::Result<Option<Self>> {
        let Some(section) = &cfg.archive else { return Ok(None) };
        let vault = cfg.custody.vault.parse().context("custody.vault")?;
        Self::new(&section.public_url, &snapshots_prefix(cfg.custody.chain_id, vault)).map(Some)
    }

    /// The snapshots under key `prefix` at the public origin `public`.
    /// Error cases:
    /// - the prefix does not join the origin
    pub(crate) fn new(public: &Url, prefix: &str) -> anyhow::Result<Self> {
        Ok(Self { base: public.join(prefix).with_context(|| format!("the public URL of {prefix}"))? })
    }

    /// The URL of `file`'s object, where `file` is a name an offset gives, so no other path.
    /// Error cases:
    /// - the file name is no offset's, or does not join the base
    pub(crate) fn object(&self, file: &str) -> anyhow::Result<Url> {
        ensure!(offset_named(file).is_some(), "{file:?} is not a daily snapshot's name");
        self.base.join(file).with_context(|| format!("the URL of {file} under {}", self.base))
    }

    /// The object a node's `308` for snapshot `file` names, which a reader reads in its place: `location` must be
    /// over `http` or `https`, name no credentials, query or fragment, and end in `file`, and be `own`'s object of it
    /// when the reader's own `[archive]` names a bucket, so a node cannot send it elsewhere.
    /// Error cases:
    /// - `location` is not such an object of `file`
    pub(crate) fn redirected(location: &Url, file: &str, own: Option<&Self>) -> anyhow::Result<Url> {
        ensure!(matches!(location.scheme(), "http" | "https"), "a redirect to {location} is not over http");
        ensure!(
            location.username().is_empty()
                && location.password().is_none()
                && location.query().is_none()
                && location.fragment().is_none(),
            "a redirect for {file} to {location} names more than an object"
        );
        ensure!(
            location.path_segments().and_then(|mut segments| segments.next_back()) == Some(file),
            "a redirect for {file} names {location}, another object"
        );
        if let Some(own) = own {
            let object = own.object(file)?;
            ensure!(*location == object, "a redirect for {file} to {location} leaves the bucket [archive] names");
        }
        Ok(location.clone())
    }
}

impl std::fmt::Display for SnapshotBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.base.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "00000000000000000042-0000000001.json";

    fn bucket(public: &str) -> SnapshotBucket {
        SnapshotBucket::new(&public.parse().unwrap(), &snapshots_prefix(31_337, AccountId([0xab; 20]))).unwrap()
    }

    /// Each snapshot's object stands under its log's chain and vault, apart from the log's ranges.
    #[test]
    fn a_snapshot_is_keyed_under_its_logs_chain_and_vault() {
        let object = bucket("https://archive.example.com").object(FILE).unwrap();
        let vault = format!("0x{}", "ab".repeat(20));
        assert_eq!(object.as_str(), format!("https://archive.example.com/snapshots/31337/{vault}/{FILE}"));
    }

    /// An object's URL is formed only from a snapshot's name, so no name climbs out of the log's prefix, reaches below
    /// it or names another origin.
    #[test]
    fn an_object_is_formed_only_from_a_snapshots_name() {
        let bucket = bucket("https://archive.example.com");
        for file in [
            format!("../{FILE}"),
            format!("../../../log/31337/{FILE}"),
            format!("sub/{FILE}"),
            format!("//other.example.com/{FILE}"),
            format!("{FILE}?x=1"),
            "00000000000000000042-0000000001.jsonl.zst".to_owned(),
            String::new(),
        ] {
            let refused = bucket.object(&file).unwrap_err().to_string();
            assert!(refused.contains("is not a daily snapshot's name"), "{file}: {refused}");
        }
    }

    /// A redirect is followed only to an object of the file it was asked for, over http, naming nothing else, and
    /// to the reader's own bucket when it names one.
    #[test]
    fn a_redirect_names_the_files_object_in_the_readers_own_bucket() {
        let own = bucket("https://archive.example.com");
        let object = own.object(FILE).unwrap();
        assert_eq!(SnapshotBucket::redirected(&object, FILE, Some(&own)).unwrap(), object);
        let elsewhere = bucket("https://other.example.com").object(FILE).unwrap();
        assert_eq!(SnapshotBucket::redirected(&elsewhere, FILE, None).unwrap(), elsewhere, "no bucket of its own");
        let refused = |location: &str, own: Option<&SnapshotBucket>| {
            SnapshotBucket::redirected(&location.parse().unwrap(), FILE, own).unwrap_err().to_string()
        };
        assert!(refused(elsewhere.as_str(), Some(&own)).contains("leaves the bucket"));
        assert!(refused(&format!("ftp://archive.example.com/{FILE}"), None).contains("not over http"));
        assert!(refused("https://archive.example.com/00000000000000000043-0000000001.json", None).contains("another"));
        assert!(refused(&format!("https://archive.example.com/{FILE}?x=1"), None).contains("more than an object"));
        assert!(refused(&format!("https://me:pw@archive.example.com/{FILE}"), None).contains("more than an object"));
    }
}
