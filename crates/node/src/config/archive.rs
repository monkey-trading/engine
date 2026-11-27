//! `[archive]`: the bucket in object storage that keeps the log, compressed, for good. The node uploads
//! each range of it through the bucket's S3 API with delivered keys, and reads it back from the bucket's
//! public origin, as every reader of the archive does (`crate::archive`).

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, ensure};
use reqwest::Url;
use serde::Deserialize;

use crate::config::SecretName;

/// The longest S3 bucket name.
const MAX_BUCKET_BYTES: usize = 63;
/// The shortest `local_window` a production build takes: four days. A start tries the three newest daily snapshots in
/// turn, and the window holds the log from the newest, so four days of daily snapshots keep each one's suffix on the
/// host; a window of hours is for a development build's scenario alone.
const MIN_PRODUCTION_WINDOW: Duration = Duration::from_secs(4 * 86_400);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveSection {
    /// The bucket's S3 API endpoint, which takes the signed uploads: for Cloudflare R2,
    /// `https://<account id>.r2.cloudflarestorage.com`. The bucket follows it in each object's path.
    pub endpoint: Url,
    /// The bucket's name.
    pub bucket: String,
    /// The region the uploads are signed for: `auto` for R2.
    pub region: String,
    /// The bucket's public origin, its custom domain, serving each object at its key. The node reads
    /// every upload back there.
    pub public_url: Url,
    /// The delivered secret holding the bucket's access key id.
    pub access_key_id_secret: SecretName,
    /// The delivered secret holding the bucket's secret access key.
    pub secret_access_key_secret: SecretName,
    /// How long a whole range of the log may wait for its upload to be confirmed, the vault's finality
    /// included, before Warning `archive_upload_lagging`.
    #[serde(with = "humantime_serde", default = "default_lag_alert")]
    pub lag_alert: Duration,
    /// How long, in sequenced time, the host keeps the log's segments once the bucket holds them: a segment whose
    /// every command is older than this, counted back from the newest, leaves the host (`archive::window`), and
    /// readers take it from the bucket. Absent keeps every segment on the host.
    #[serde(default, with = "humantime_serde::option")]
    pub local_window: Option<Duration>,
}

impl ArchiveSection {
    /// Checks the bucket's addresses and names. The endpoint and the public origin are each an
    /// origin alone, so a key's path cannot reach another bucket or host, and travel over `https`,
    /// or plain `http` to a loopback stand-in under the mock TEE (`mock_tee`).
    /// Error cases:
    /// - the endpoint or the public origin carries a path, a query, a fragment or credentials, or is
    ///   not `https` but under the mock TEE to a loopback address
    /// - the bucket is not an S3 bucket name, or the region is empty or not letters, digits and `-`
    /// - both keys name one secret, or `lag_alert` is zero
    pub fn validate(&self, mock_tee: bool) -> anyhow::Result<()> {
        origin_only(&self.endpoint, mock_tee).context("archive.endpoint")?;
        origin_only(&self.public_url, mock_tee).context("archive.public_url")?;
        let legal = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.';
        ensure!(
            (3..=MAX_BUCKET_BYTES).contains(&self.bucket.len())
                && self.bucket.bytes().all(legal)
                && self.bucket.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
                && self.bucket.bytes().last().is_some_and(|b| b.is_ascii_alphanumeric()),
            "archive.bucket must be 3 to {MAX_BUCKET_BYTES} lowercase letters, digits, '-' and '.', starting and \
             ending with a letter or digit"
        );
        ensure!(
            !self.region.is_empty() && self.region.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "archive.region must be letters, digits and '-'"
        );
        ensure!(
            self.access_key_id_secret != self.secret_access_key_secret,
            "archive.access_key_id_secret and archive.secret_access_key_secret must name different secrets"
        );
        ensure!(!self.lag_alert.is_zero(), "archive.lag_alert must be non-zero");
        Ok(())
    }

    /// Checks `local_window` against the sections it rests on: the daily snapshots, whose newest usable one bounds
    /// what leaves the host (`daily`), and the history index, whose `retention` the window must cover, since the
    /// index re-derives a retained command's rows from the log on the host. A production build takes no window under
    /// four days.
    /// Error cases:
    /// - a window without `[daily_snapshots]`, shorter than `retention`, or zero
    /// - a window under `MIN_PRODUCTION_WINDOW` in a `production` build
    pub fn validate_window(&self, daily: bool, retention: Duration, production: bool) -> anyhow::Result<()> {
        let Some(window) = self.local_window else { return Ok(()) };
        ensure!(
            daily,
            "archive.local_window needs [daily_snapshots]: a start goes on from the newest daily snapshot and the log \
             after it, so no segment after it leaves the host"
        );
        ensure!(!window.is_zero(), "archive.local_window must be non-zero");
        ensure!(
            window >= retention,
            "archive.local_window must be at least history.retention, {retention:?}, so the host keeps every command \
             the history index holds"
        );
        ensure!(
            !production || window >= MIN_PRODUCTION_WINDOW,
            "archive.local_window must be at least {MIN_PRODUCTION_WINDOW:?} in a production build"
        );
        Ok(())
    }

    /// The fields that name the bucket's keys, with their names.
    pub(crate) fn secret_fields(&self) -> [(&'static str, &SecretName); 2] {
        [
            ("access_key_id_secret", &self.access_key_id_secret),
            ("secret_access_key_secret", &self.secret_access_key_secret),
        ]
    }
}

/// An hour: the time a whole range may wait for its upload to be confirmed by default.
fn default_lag_alert() -> Duration {
    Duration::from_secs(3600)
}

/// Refuses a URL that is more than an origin, or not `https` but under the mock TEE to a loopback
/// address.
/// Error cases:
/// - see `ArchiveSection::validate`
fn origin_only(url: &Url, mock_tee: bool) -> anyhow::Result<()> {
    ensure!(
        url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
        "must be an origin alone, with no path, query or fragment"
    );
    ensure!(url.username().is_empty() && url.password().is_none(), "must carry no credentials");
    let loopback = url
        .host_str()
        .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback());
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback && mock_tee),
        "must be https, or plain http to a loopback stand-in under the mock TEE"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A section as the release configs write it.
    fn section(endpoint: &str, public_url: &str) -> ArchiveSection {
        toml::from_str(&format!(
            "endpoint = \"{endpoint}\"\nbucket = \"engine-log\"\nregion = \"auto\"\npublic_url = \"{public_url}\"\n\
             access_key_id_secret = \"archive_key_id\"\nsecret_access_key_secret = \"archive_secret_key\"\n"
        ))
        .expect("a section")
    }

    #[test]
    fn an_https_section_validates_with_an_hour_to_lag() {
        let section = section("https://account.r2.cloudflarestorage.com", "https://archive.example.com");
        section.validate(false).expect("valid");
        assert_eq!(section.lag_alert, Duration::from_secs(3600));
        assert_eq!(section.local_window, None, "every segment stays on the host by default");
    }

    #[test]
    fn a_window_needs_the_daily_snapshots_and_covers_the_historys_retention() {
        let mut section = section("https://account.r2.cloudflarestorage.com", "https://archive.example.com");
        let (day, month) = (Duration::from_secs(86_400), Duration::from_secs(30 * 86_400));
        section.local_window = Some(3 * day);
        let refused = section.validate_window(true, day, true).expect_err("three days in production");
        assert!(refused.to_string().contains("production build"), "{refused:#}");
        section.local_window = Some(4 * day);
        section.validate_window(true, day, true).expect("four days in production");
        section.local_window = None;
        section.validate_window(false, month, true).expect("no window needs nothing");
        section.local_window = Some(month);
        section.validate_window(true, month, true).expect("thirty days over a thirty-day history");
        let refused = section.validate_window(false, month, true).expect_err("no daily snapshots");
        assert!(refused.to_string().contains("[daily_snapshots]"), "{refused:#}");
        let refused = section.validate_window(true, month + day, true).expect_err("shorter than the history");
        assert!(refused.to_string().contains("history.retention"), "{refused:#}");
        section.local_window = Some(Duration::from_secs(4 * 3600));
        section.validate_window(true, Duration::from_secs(3600), false).expect("hours in a development build");
        let refused = section.validate_window(true, Duration::from_secs(3600), true).expect_err("hours in production");
        assert!(refused.to_string().contains("production build"), "{refused:#}");
        section.local_window = Some(Duration::ZERO);
        assert!(section.validate_window(true, Duration::ZERO, false).is_err(), "a zero window");
    }

    #[test]
    fn a_node_config_with_a_window_and_no_daily_snapshots_is_refused_at_load() {
        let mut cfg = crate::test_support::dev_config();
        let mut archive = section("http://127.0.0.1:9000", "http://127.0.0.1:9000");
        archive.local_window = Some(cfg.history.retention);
        (cfg.archive, cfg.daily_snapshots) = (Some(archive), None);
        let refused = cfg.validate().expect_err("a window with no daily snapshots");
        assert!(format!("{refused:#}").contains("archive.local_window needs [daily_snapshots]"), "{refused:#}");
    }

    #[test]
    fn a_window_parses_as_a_duration() {
        let parsed: ArchiveSection = toml::from_str(
            "endpoint = \"https://a.example.com\"\nbucket = \"engine-log\"\nregion = \"auto\"\n\
             public_url = \"https://b.example.com\"\naccess_key_id_secret = \"a\"\nsecret_access_key_secret = \"b\"\n\
             local_window = \"30d\"\n",
        )
        .expect("a section");
        assert_eq!(parsed.local_window, Some(Duration::from_secs(30 * 86_400)));
    }

    #[test]
    fn a_url_that_is_more_than_an_origin_is_refused() {
        for (endpoint, public) in [
            ("https://account.r2.cloudflarestorage.com/other-bucket", "https://archive.example.com"),
            ("https://account.r2.cloudflarestorage.com", "https://archive.example.com/log"),
            ("https://account.r2.cloudflarestorage.com", "https://archive.example.com/?x=1"),
            ("https://user:pass@account.r2.cloudflarestorage.com", "https://archive.example.com"),
        ] {
            assert!(section(endpoint, public).validate(false).is_err(), "{endpoint} {public}");
        }
    }

    #[test]
    fn plain_http_is_only_for_a_loopback_stand_in_under_the_mock_tee() {
        let local = section("http://127.0.0.1:9000", "http://127.0.0.1:9000");
        local.validate(true).expect("a loopback stand-in under the mock TEE");
        assert!(local.validate(false).is_err(), "refused with a real TEE");
        let remote = section("http://account.r2.cloudflarestorage.com", "https://archive.example.com");
        assert!(remote.validate(true).is_err(), "plain http beyond loopback");
    }

    #[test]
    fn bucket_region_keys_and_lag_are_checked() {
        let mut section = section("https://account.r2.cloudflarestorage.com", "https://archive.example.com");
        for bucket in ["ab", "Engine-Log", "-engine", "engine_log"] {
            section.bucket = bucket.to_owned();
            assert!(section.validate(false).is_err(), "bucket {bucket}");
        }
        section.bucket = "engine-log".to_owned();
        section.region = String::new();
        assert!(section.validate(false).is_err(), "an empty region");
        section.region = "auto".to_owned();
        section.secret_access_key_secret = section.access_key_id_secret.clone();
        assert!(section.validate(false).is_err(), "one secret for both keys");
        section.secret_access_key_secret = "archive_secret_key".to_owned().try_into().unwrap();
        section.lag_alert = Duration::ZERO;
        assert!(section.validate(false).is_err(), "a zero lag");
    }
}
