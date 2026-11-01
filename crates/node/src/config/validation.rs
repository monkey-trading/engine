//! The checks a node config must pass once it parses: values that would fail silently or at the
//! wrong moment.

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use tracing::level_filters::LevelFilter;
use tracing::subscriber::with_default;
use tracing::{Level, enabled};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, registry};

use crate::config::{
    AuthMode, CustodyProvider, FeedProvider, MIN_QUOTE_INTERVAL, NodeConfig, PRODUCTION_BUILD, TeeProvider,
};
use crate::sequencer::genesis::check_genesis;
use crate::trusted_time::NO_SAMPLE_GRACE;

/// The least `feeds.venues.min_quote_spacing`: the most often one symbol's latest quote may be
/// sequenced. Venue message rates stay well under it, while an injected host flooding the
/// socket cannot make a feed sequence a symbol's quotes more often than 100 a second, whatever
/// the config a deployment attests.
const MIN_VENUE_QUOTE_SPACING: Duration = Duration::from_millis(10);

impl NodeConfig {
    /// Refuses every mock provider when `production` is set, whatever else the config says.
    /// Error cases:
    /// - `production` is set and `tee.provider`, `custody.provider`, or `feeds.provider` is `mock`
    pub(super) fn refuse_mocks(&self, production: bool) -> anyhow::Result<()> {
        let mocks: Vec<&str> = [
            ("tee.provider", self.tee.provider == TeeProvider::Mock),
            ("custody.provider", self.custody.provider == CustodyProvider::Mock),
            ("feeds.provider", self.feeds.provider == FeedProvider::Mock),
        ]
        .into_iter()
        .filter_map(|(key, is_mock)| is_mock.then_some(key))
        .collect();
        if production && !mocks.is_empty() {
            bail!("a production build refuses mock providers: {} set to \"mock\"", mocks.join(", "));
        }
        Ok(())
    }

    /// Checks values that parse but would fail silently or at the wrong moment.
    /// Error cases:
    /// - `logging.level` is not a valid filter directive, or one that disables the node's errors
    /// - `node.tick`, `node.checkpoint_every`, `node.segment_commands`, `feeds.interval`,
    ///   `time.max_drift`, `time.interval`, or an `api` timeout is zero, or `time.interval` plus
    ///   twice `time.max_drift` is not under 60s
    /// - `api.heartbeat_interval` is under one second
    /// - `api.quote_interval` is under 100 ms
    /// - `node.auth` is `none` with a TEE or custody provider other than `mock`
    /// - `tee.provider` is not `mock` and `operator.public_key` is a published test key (see
    ///   `refuse_test_keys`)
    /// - `[alerts.critical]` is missing with a TEE or custody provider other than `mock`
    /// - `feeds.venues` is missing for `provider = "venues"`, fails validation, or has a
    ///   `min_quote_spacing` outside `10ms..feeds.venues.timeout`
    /// - `time.servers` has fewer than two servers, lists a public key twice, or has an address
    ///   that is not `host:port`
    /// - `api.allowed_origins` lists a wildcard, a malformed origin, or one twice (see
    ///   `validate_origins`)
    /// - `history.retention` is zero, or too long to count in milliseconds
    /// - `[history.marks]` fails validation (see `MarksSection::validate`)
    /// - the custody section does not match its provider (see `validate_custody`)
    /// - `operator.public_key` or `custody.vault` does not parse
    /// - genesis lists a market id, a market symbol, or a benchmark name twice
    /// - the engine would reject a genesis command (see `check_genesis`)
    /// - `[api.tls.acme]` fails validation (see `AcmeSection::validate`)
    /// - `[api.proxy_protocol]` fails validation (see `ProxyProtocolSection::validate`)
    /// - `[tee.sealing]` fails validation (see `validate_sealing`)
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        if let Some(access) = &self.access {
            access.validate()?;
            if self.tee.provider != TeeProvider::Mock {
                ensure!(access.endpoint.expose()?.scheme() == "https", "access HTTP fixtures require the mock TEE");
            }
        }
        if let Some(benchmarks) = &self.benchmarks {
            benchmarks.validate(self)?;
        }
        self.tee.validate()?;
        self.validate_sealing()?;
        let filter = EnvFilter::try_new(&self.logging.level).context("logging.level")?;
        // A word that is not a level, such as "warning", parses as a target and hides every alert.
        ensure!(
            enables_node_errors(&self.logging.level, filter),
            "logging.level must enable at least \"error\" for engine_node and every module in it"
        );
        if self.node.tick.is_zero()
            || self.node.checkpoint_every == 0
            || self.node.segment_commands == 0
            || self.feeds.interval.is_zero()
        {
            bail!("node.tick, node.checkpoint_every, node.segment_commands, and feeds.interval must be non-zero");
        }
        match &self.feeds.venues {
            Some(venues) => {
                venues.validate().context("feeds.venues")?;
                // Silence is checked once a spacing, so a longer spacing would outlast the timeout.
                ensure!(
                    venues.min_quote_spacing < venues.timeout,
                    "feeds.venues.min_quote_spacing must be shorter than feeds.venues.timeout"
                );
                // The pace a symbol's quotes sequence at, so an injected flood of venue messages
                // cannot amplify sequencer load past a hundred quotes a second per symbol.
                ensure!(
                    venues.min_quote_spacing >= MIN_VENUE_QUOTE_SPACING,
                    "feeds.venues.min_quote_spacing must be at least {MIN_VENUE_QUOTE_SPACING:?}, \
                     so a symbol's quotes sequence at most 100 times a second"
                );
            }
            None if self.feeds.provider == FeedProvider::Venues => {
                bail!("feeds.provider = \"venues\" needs [feeds.venues]")
            }
            None => {}
        }
        if [self.api.handshake_timeout, self.api.read_timeout, self.api.send_timeout].iter().any(Duration::is_zero) {
            anyhow::bail!("api.handshake_timeout, api.read_timeout, and api.send_timeout must be non-zero");
        }
        // A shorter interval would have every signed subscriber sign heartbeats faster than a client
        // needs them.
        ensure!(self.api.heartbeat_interval >= Duration::from_secs(1), "api.heartbeat_interval must be at least 1s");
        // Every quote read runs on the sequencer's thread, so the interval bounds how much of it
        // one connection can take.
        ensure!(self.api.quote_interval >= MIN_QUOTE_INTERVAL, "api.quote_interval must be at least 100ms");
        self.api.validate_origins()?;
        self.api.validate_proxy_protocol()?;
        if let Some(tls) = &self.api.tls {
            tls.acme.validate(self.tee.provider)?;
        }
        // Unsigned requests let anyone act for any account, so they never run beside a real
        // enclave or real funds.
        if self.node.auth == AuthMode::None
            && (self.tee.provider != TeeProvider::Mock || self.custody.provider != CustodyProvider::Mock)
        {
            anyhow::bail!("node.auth = \"none\" needs tee.provider and custody.provider both \"mock\"");
        }
        self.refuse_test_keys()?;
        ensure!(
            !self.tee.use_published_mock_key || self.tee.provider == TeeProvider::Mock,
            "tee.use_published_mock_key requires tee.provider = \"mock\""
        );
        if self.time.max_drift.is_zero() || self.time.interval.is_zero() {
            anyhow::bail!("time.max_drift and time.interval must be non-zero");
        }
        // A round waits up to `max_drift` for a silent server, so a sample may be that old when it
        // arrives, and the next may come `interval` plus `max_drift` later. All of it must fit in
        // the grace, or a node with one silent server halts while the others agree.
        let round = self.time.interval.saturating_add(self.time.max_drift.saturating_mul(2));
        if round >= NO_SAMPLE_GRACE {
            anyhow::bail!("time.interval plus twice time.max_drift must be under {NO_SAMPLE_GRACE:?}");
        }
        // One server, or one key listed twice, would let a single operator set the time.
        let servers = &self.time.servers;
        if servers.len() < 2 {
            anyhow::bail!("time.servers needs at least two servers, so that more than one must agree");
        }
        if servers.iter().enumerate().any(|(i, a)| servers[..i].iter().any(|b| a.public_key == b.public_key)) {
            anyhow::bail!("time.servers lists the same public key twice");
        }
        if let Some(s) = servers.iter().find(|s| !is_host_port(&s.address)) {
            anyhow::bail!("time.servers address {:?} is not host:port", s.address);
        }
        self.alerts.outbox.validate()?;
        self.alerts.outbox.validate_location(&self.node.log_dir)?;
        self.history.retention_ms().context("history.retention")?;
        if let Some(marks) = &self.history.marks {
            marks.validate(PRODUCTION_BUILD).context("history.marks")?;
        }
        self.validate_custody()?;
        self.validate_checkpoints()?;
        // Without a Critical webhook, Critical alerts reach only the node's own log, so a node
        // beside a real enclave or real funds must have one.
        if self.alerts.critical.is_none()
            && (self.tee.provider != TeeProvider::Mock || self.custody.provider != CustodyProvider::Mock)
        {
            anyhow::bail!(
                "[alerts.critical] is required unless tee.provider and custody.provider are both \"mock\": \
                 without it Critical alerts page no one"
            );
        }
        // The engine replaces a market or benchmark rate set twice, so a repeated id or name would
        // silently drop the earlier entry, and a repeated symbol would name two markets.
        let markets = &self.genesis.markets;
        if let Some(id) = repeated(markets.iter().map(|m| m.id)) {
            bail!("genesis lists market {id} twice");
        }
        if let Some(symbol) = repeated(markets.iter().map(|m| m.symbol.as_str())) {
            bail!("genesis lists market symbol {symbol} twice");
        }
        if let Some(name) = repeated(self.genesis.benchmarks.iter().map(|b| b.name.as_str())) {
            bail!("genesis lists benchmark {name} twice");
        }
        // Markets listed at genesis need an adopted close-spread ceiling each: the engine refuses
        // an incomplete registry, and a log without one runs `V40` uncapped. (The engine's genesis
        // check names the offending entry.)
        if !self.genesis.markets.is_empty() && self.genesis.close_spread_ceilings.is_empty() {
            bail!("genesis lists markets but no close-spread ceilings");
        }
        if let Some(id) = repeated(self.genesis.close_spread_ceilings.iter().map(|c| c.market)) {
            bail!("genesis lists a close-spread ceiling for market {id} twice");
        }
        // Genesis is all or nothing, so a command the engine would refuse fails the load.
        check_genesis(&self.client_auth()?, self.operator()?, &self.genesis)
    }
}

/// Whether `filter`, parsed from `level`, enables error events from every module of the node, as
/// every Critical alert is one: it enables them for the crate, and no static directive in `level`
/// turns off a target inside it.
fn enables_node_errors(level: &str, filter: EnvFilter) -> bool {
    let turns_off_a_module =
        level.split(',').filter_map(|directive| directive.rsplit_once('=')).any(|(spec, level)| {
            let (target, selector) = spec.split_once('[').unwrap_or((spec, ""));
            let target = target.trim();
            // A target matches every event target it prefixes, so `engine_node:` reaches every module.
            let in_node = "engine_node::".starts_with(target) || target.starts_with("engine_node::");
            // A span name or a field value makes a directive dynamic: it can enable events in its
            // scope, never disable what the static directives enable.
            let (span, fields) = selector.split_once('{').unwrap_or((selector.trim_end_matches(']'), ""));
            let dynamic = !span.trim().is_empty() || fields.contains('=');
            in_node && !dynamic && level.trim().parse::<LevelFilter>().is_ok_and(|level| level == LevelFilter::OFF)
        });
    !turns_off_a_module && with_default(registry().with(filter), || enabled!(target: "engine_node", Level::ERROR))
}

/// The first item `items` repeats, if any.
fn repeated<T: Ord + Copy>(items: impl IntoIterator<Item = T>) -> Option<T> {
    let mut seen = BTreeSet::new();
    items.into_iter().find(|item| !seen.insert(*item))
}

/// Whether `address` is a non-empty host, a colon, and a port number.
fn is_host_port(address: &str) -> bool {
    address.rsplit_once(':').is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
}
