//! This start's delivered secrets. A release opens what it sealed, a new one first asks the running
//! release on `--predecessor`, and then the node holds, serving only its delivery listener, until it
//! has every secret it cannot safely run without. It sets what it has into the config and the alert
//! routes, and leaves each integration whose secret it lacks off, under `secret_missing`. Nothing
//! falls back to the environment or to a default: a field that names a secret reads only that.
//! While it holds it takes only the key its attested config names: reading the vault can take a
//! delivered key, so no log it could read a rotation from can be authenticated yet. A rotated
//! operator keeps its old key until a release's config names the new one.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use engine_core::types::AccountId;
use futures_util::future::BoxFuture;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::{Instant, Interval, MissedTickBehavior, interval_at, sleep, timeout};
use tracing::{error, info, warn};

use crate::config::{ApiKey, NamedSecret, NodeConfig, SecretUse, Values, VenueConnection};
use crate::handover::fetch_secrets;
use crate::node::keys::{KeyStart, attestor_for, sealer};
use crate::secrets::{Authority, Listener, SignedBundle};
use crate::tee::clock::GuestClock;
use crate::tee::secrets::{Replaced, SecretStore};
use crate::telemetry::Telemetry;

/// How often a failed start looks for a new bundle.
const NEW_BUNDLE_POLL: Duration = Duration::from_millis(500);
/// How long a failed start then waits for that bundle's receipt, whose quote fetches its collateral.
const RECEIPT_WAIT: Duration = Duration::from_secs(60);
/// How many failed attempts on `--predecessor`, refusals included since a refusal can be a read that
/// failed over there, a start with no required secret outstanding waits through before it goes on to
/// take the keys without the bundle.
const PREDECESSOR_ATTEMPTS: u32 = 3;
/// How often a holding node asks `--predecessor` again.
const PREDECESSOR_RETRY: Duration = if cfg!(test) { Duration::from_millis(50) } else { Duration::from_secs(30) };

/// What a start's secrets gave it.
#[derive(Default)]
pub(crate) struct Delivered {
    /// The secrets it runs with.
    pub values: Values,
    /// The bundle version they came from, which a failed start waits to see replaced.
    pub version: Option<u64>,
    /// The newest version whose connection has finished answering (see `Listener::answered`).
    pub answered: Option<watch::Receiver<Option<u64>>>,
    /// Whether the first read of the chain uses a delivered key, which a provider may refuse.
    pub chain_read_keyed: bool,
    /// The delivery listener, with `[tee.sealing.secrets]`, which the node runs as a supervised task.
    pub listener: Option<Listener>,
    /// Sets who the listener takes deliveries from once the sequencer can say.
    pub authority: Option<watch::Sender<Authority>>,
    /// The sealed bundle, which an approved upgrade takes.
    pub store: Option<Arc<SecretStore>>,
}

/// Moving the running release's secrets here on `--predecessor`.
struct Moving<'a> {
    cfg: &'a NodeConfig,
    address: &'a str,
    start: &'a KeyStart,
    store: &'a SecretStore,
    operator: AccountId,
    named: &'a [NamedSecret],
    retry: Interval,
    attempt: Option<BoxFuture<'a, anyhow::Result<Replaced>>>,
    failures: u32,
    /// Whether the keys' handover may go on: see `unsettled`.
    settled: bool,
}

impl Delivered {
    /// Whether `connection` names a key this start lacks, so it stays off.
    pub(crate) fn withholds(&self, connection: &VenueConnection) -> bool {
        connection.api_key_secret.as_ref().is_some_and(|name| !self.values.contains_key(name))
    }

    /// `connection`'s API key: the delivered one it names, or the variable it names.
    /// Error cases:
    /// - a named variable is unset or empty
    pub(crate) fn api_key(&self, connection: &VenueConnection) -> anyhow::Result<Option<ApiKey>> {
        match &connection.api_key_secret {
            Some(name) => Ok(self.values.get(name).map(ApiKey::delivered)),
            None => connection.api_key(|var| std::env::var(var).ok()),
        }
    }
}

/// Waits until a new bundle is sealed and its connection has answered, or until `hold` has passed or
/// the listener has stopped: the listener takes deliveries meanwhile, and the caller exits so the next
/// start takes what it sealed.
pub(crate) async fn wait_for_a_new_bundle(
    delivered: &Delivered,
    listening: &mut JoinSet<anyhow::Result<()>>,
    hold: Duration,
) {
    let Some(store) = &delivered.store else { return };
    let sealed = async {
        // Against what this start read with: a bundle sealed while that read failed is the repair.
        while store.version() == delivered.version {
            sleep(NEW_BUNDLE_POLL).await;
        }
        // The exit powers the guest off, which would leave the operator no receipt and nothing to
        // retry on.
        if let Some(answered) = &delivered.answered {
            let (mut answered, sealed) = (answered.clone(), store.version());
            let _ = timeout(RECEIPT_WAIT, answered.wait_for(|newest| *newest >= sealed)).await;
        }
    };
    tokio::select! {
        () = sealed => {}
        () = sleep(hold) => {}
        _ = listening.join_next() => {}
    }
}

/// Opens or waits for this release's secrets, and sets them into `cfg` and `telemetry`.
/// Error cases:
/// - the platform cannot report (see `SecretStore::open`), the stand-in measurement is missing under
///   the mock TEE, or the listener cannot be bound
/// - a URL's `{name}` is malformed, which the config load refused
pub(crate) async fn startup(
    cfg: &mut NodeConfig,
    telemetry: &mut Telemetry,
    start: &KeyStart,
) -> anyhow::Result<Delivered> {
    let Some(sealing) = &cfg.tee.sealing else { return Ok(Delivered::default()) };
    let Some(section) = &sealing.secrets else { return Ok(Delivered::default()) };
    let named = cfg.named_secrets()?;
    let platform = sealer(cfg.tee.provider, start)?.platform()?;
    let store = Arc::new(SecretStore::open(&sealing.dir, sealer(cfg.tee.provider, start)?, cfg.file_sha256)?);
    if let Some(reason) = store.unreadable() {
        error!(
            alert = "secrets_unreadable",
            "this release's sealed secrets do not open, so it runs as if it had none until a delivery replaces them: \
             {reason}"
        );
    }
    let attestor = attestor_for(cfg.tee.provider, &GuestClock::default())
        .inspect_err(|e| error!(alert = "clocksource_unprotected", "refusing to serve: {e:#}"))?;
    let operator = cfg.operator()?.address;
    let (authority, taking) = watch::channel(Authority::Configured(operator));
    let listener =
        Listener::bind(cfg, section.listen, platform, attestor, store.clone(), named.clone(), taking).await?;
    let mut serving = JoinSet::new();
    let mut moving =
        start.predecessor.as_deref().map(|address| Moving::new(cfg, address, start, &store, operator, &named));
    let mut raised = false;
    // The completeness check and the choice of what runs take one store lock, so a delivery cannot
    // seal a partial bundle over the complete one between them; a bundle that fails the check under
    // the lock keeps this start holding.
    let held = loop {
        let lacking = lacks_required(&store, &named);
        if lacking.is_empty() && moving.as_ref().is_none_or(|moving| !moving.unsettled()) {
            if let Some(held) = store.current_if_complete(
                |signed| missing_required(signed, &named).is_empty(),
                |version| listener.started(version),
            ) {
                break held;
            }
            raised = false;
        }
        // A new release has asked for what it lacks, and a prompt answer is the ordinary upgrade.
        let asked = moving.as_ref().is_none_or(|moving| moving.failures > 0 || moving.settled);
        if !raised && asked && !lacking.is_empty() {
            report_missing(store.current().as_ref(), &named);
            raised = true;
        }
        match &mut moving {
            Some(moving) => tokio::select! {
                () = listener.step(&mut serving) => {}
                () = moving.step(&lacking) => {}
            },
            None => listener.step(&mut serving).await,
        }
    };
    drop(moving);
    // Whatever else is connected finishes on its own deadlines, its receipt included.
    serving.detach_all();
    // From what this start runs: a delivery still connected may already have sealed a newer one.
    report_missing(held.as_ref(), &named);
    let version = held.as_ref().map(|signed| signed.bundle.version);
    let values = held.map(|signed| signed.bundle.secrets).unwrap_or_default();
    cfg.deliver_urls(&values).context("setting the delivered keys")?;
    match telemetry.alerts.deliver_routes(&cfg.alerts, &values) {
        Ok(()) => info!(alert_resolved = "secret_unusable", "every delivered webhook URL is acceptable"),
        Err(e) => error!(
            alert = "secret_unusable",
            "a delivered webhook URL is not acceptable, so its route stays off: {e:#}"
        ),
    }
    telemetry.pages_critical = telemetry.alerts.pages_critical();
    if store.unreadable().is_none() {
        info!(alert_resolved = "secrets_unreadable", "this release's sealed secrets open");
    }
    info!(?version, delivered = values.len(), named = named.len(), "the delivered secrets are set");
    let chain_read_keyed = cfg.checkpoints.as_ref().map_or(Ok(false), |section| section.rpc_url.names_a_secret())?;
    Ok(Delivered {
        values,
        version,
        answered: Some(listener.answered()),
        chain_read_keyed,
        listener: Some(listener),
        authority: Some(authority),
        store: Some(store),
    })
}

/// The required secrets the bundle `held` lacks, each once.
fn missing_required(held: Option<&SignedBundle>, named: &[NamedSecret]) -> Vec<NamedSecret> {
    named
        .iter()
        .filter(|field| {
            field.usage == SecretUse::Required
                && !held.is_some_and(|signed| signed.bundle.secrets.contains_key(&field.name))
        })
        .cloned()
        .collect()
}

/// The required secrets `store` lacks, each once.
fn lacks_required(store: &SecretStore, named: &[NamedSecret]) -> Vec<NamedSecret> {
    missing_required(store.current().as_ref(), named)
}

/// Raises `secret_missing` for each field whose secret `held` lacks, and resolves it for each it holds.
fn report_missing(held: Option<&SignedBundle>, named: &[NamedSecret]) {
    for field in named {
        let (secret, field_name) = (&field.name, &field.field);
        if held.is_some_and(|signed| signed.bundle.secrets.contains_key(secret)) {
            info!(alert_resolved = "secret_missing", %secret, field = %field_name, "the field has its secret");
            continue;
        }
        let effect = match field.usage {
            SecretUse::Required => "the node holds, serving only its delivery listener, until it is delivered",
            SecretUse::VendorKey => "this vendor connection stays off, and the markets it alone prices go unpriced",
            SecretUse::WebhookUrl => "this alert route stays off, so its alerts are only logged",
            SecretUse::SeedRelay => "the seed relayer stays off",
        };
        error!(alert = "secret_missing", %secret, field = %field_name, "no delivered bundle holds this secret: {effect}");
    }
}

impl<'a> Moving<'a> {
    /// The first attempt to take the running release's secrets, from `address`.
    fn new(
        cfg: &'a NodeConfig,
        address: &'a str,
        start: &'a KeyStart,
        store: &'a SecretStore,
        operator: AccountId,
        named: &'a [NamedSecret],
    ) -> Self {
        let mut retry = interval_at(Instant::now() + PREDECESSOR_RETRY, PREDECESSOR_RETRY);
        retry.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut moving =
            Self { cfg, address, start, store, operator, named, retry, attempt: None, failures: 0, settled: false };
        moving.attempt = Some(moving.attempt());
        moving
    }

    /// Whether the keys' handover, which ends the release holding the bundle, must wait: until it has
    /// moved the bundle, or a few attempts have shown it will not.
    fn unsettled(&self) -> bool {
        !self.settled
    }

    /// One attempt, as its own future, so the hold goes on taking deliveries while it runs. A hung
    /// predecessor can take minutes.
    fn attempt(&self) -> BoxFuture<'a, anyhow::Result<Replaced>> {
        let (cfg, address, start, store, operator, named) =
            (self.cfg, self.address, self.start, self.store, self.operator, self.named);
        Box::pin(async move {
            let attestor = attestor_for(cfg.tee.provider, &GuestClock::default())?;
            let sealer = sealer(cfg.tee.provider, start)?;
            let signed = fetch_secrets(cfg, address, sealer.as_ref(), attestor.as_ref(), operator, named).await?;
            store.replace(signed)
        })
    }

    /// Runs the attempt in flight to its end and settles what it shows, or waits to start the next
    /// while `lacking` shows a required secret still missing or the last one failed. Cancel-safe: the
    /// attempt stays in `self` when the hold races this against a connection.
    async fn step(&mut self, lacking: &[NamedSecret]) {
        let Some(attempt) = &mut self.attempt else {
            if self.settled && lacking.is_empty() {
                return std::future::pending().await;
            }
            self.retry.tick().await;
            self.attempt = Some(self.attempt());
            return;
        };
        let outcome = attempt.await;
        self.attempt = None;
        let address = self.address;
        match outcome {
            Ok(Replaced::Sealed) => {
                info!(predecessor = address, version = ?self.store.version(), "sealed the running release's secrets");
                self.settled = true;
            }
            Ok(Replaced::AlreadySealed) => {
                info!(predecessor = address, "the running release's secrets are already sealed");
                self.settled = true;
            }
            Err(e) => {
                warn!(predecessor = address, "taking the running release's secrets failed: {e:#}");
                self.failures += 1;
                self.settled |= self.failures >= PREDECESSOR_ATTEMPTS;
            }
        }
    }
}

#[cfg(test)]
mod tests;
