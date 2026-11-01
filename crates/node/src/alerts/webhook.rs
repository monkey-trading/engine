//! Webhook delivery: each routed severity has its own queue and task, which POSTs its pages as JSON
//! in order, retrying a page until it is delivered, refused, or has failed for an hour since its
//! first attempt.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use prometheus_client::metrics::counter::Counter;
use reqwest::redirect::Policy;
use reqwest::{Client, StatusCode};
use rustls::{ClientConfig, RootCertStore};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, timeout};
use tracing::{debug, error, info, warn};

use crate::alerts::outbox::Outbox;
use crate::alerts::{Page, Severity};
use crate::config::{AlertsSection, WebhookRoute, WebhookUrl};
use crate::metrics::{DeliveryLabels, Metrics, SeverityLabel};

/// Deadline for one POST, connection included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Retries for production: from 1s, doubling to a minute, for up to an hour from the first attempt.
pub const RETRY: Retry =
    Retry { first: Duration::from_secs(1), max: Duration::from_secs(60), give_up_after: Duration::from_secs(3600) };

/// The webhook for each severity; a severity without one is only logged.
#[derive(Debug, Default)]
pub struct Routes {
    critical: Option<WebhookUrl>,
    warning: Option<WebhookUrl>,
}

/// How a page is retried after a failure retrying can fix: the wait starts at `first` and doubles
/// up to `max`, until the page has gone undelivered for `give_up_after`.
#[derive(Clone, Copy, Debug)]
pub struct Retry {
    pub first: Duration,
    pub max: Duration,
    pub give_up_after: Duration,
}

/// Coalesced wakeups for each routed severity, backed by the durable outbox.
pub struct Queues {
    pub(super) critical: Option<mpsc::Sender<()>>,
    pub(super) warning: Option<mpsc::Sender<()>>,
    pub(super) outbox: Outbox,
}

impl Routes {
    /// Reads each configured route's URL from the environment variable it names, through `env`.
    /// Error cases:
    /// - a configured route's variable is unset, or does not hold an acceptable URL
    pub fn from_config(cfg: &AlertsSection, env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        Self::from_routes(&cfg.critical, &cfg.warning, env)
    }

    /// Reads the Critical and Warning routes' URLs as `from_config` does.
    /// Error cases:
    /// - a configured route's variable is unset, or does not hold an acceptable URL
    pub fn from_routes(
        critical: &Option<WebhookRoute>,
        warning: &Option<WebhookRoute>,
        env: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Self> {
        let url = |route: &Option<WebhookRoute>, key: &str| {
            route
                .as_ref()
                .map(|route| WebhookUrl::from_env(&route.url_env, &env).with_context(|| format!("alerts.{key}")))
                .transpose()
        };
        Ok(Self { critical: url(critical, "critical")?, warning: url(warning, "warning")? })
    }

    /// Whether `severity` has a webhook.
    pub fn has(&self, severity: Severity) -> bool {
        match severity {
            Severity::Critical => self.critical.is_some(),
            Severity::Warning => self.warning.is_some(),
        }
    }
}

impl Queues {
    /// Spawns one delivery task per route, returning its coalesced wakeup sender.
    pub fn start(
        routes: Routes,
        client: Client,
        metrics: &Arc<Metrics>,
        retry: Retry,
        outbox: Outbox,
        tasks: &mut JoinSet<()>,
    ) -> Self {
        let mut start = |severity, url: Option<WebhookUrl>| {
            url.map(|url| {
                let (tx, rx) = mpsc::channel(1);
                let worker = outbox.clone();
                let metrics = metrics.clone();
                let client = client.clone();
                tasks.spawn(async move {
                    if let Err(e) = deliver(rx, worker, severity, client, url, metrics, retry).await {
                        error!(severity = severity.as_str(), "alert delivery stopped: {e:#}");
                    }
                });
                tx
            })
        };
        Queues {
            critical: start(Severity::Critical, routes.critical),
            warning: start(Severity::Warning, routes.warning),
            outbox,
        }
    }

    /// The queue for `severity`, if it has a route.
    pub fn get(&self, severity: Severity) -> Option<&mpsc::Sender<()>> {
        match severity {
            Severity::Critical => self.critical.as_ref(),
            Severity::Warning => self.warning.as_ref(),
        }
    }
}

/// The webhook HTTP client. TLS runs on rustls with the compiled-in Mozilla roots, so the host's
/// trust store plays no part; it uses no proxy, follows no redirect, and gives every request a
/// deadline.
/// Error cases:
/// - the TLS or HTTP client cannot be built
pub fn client() -> anyhow::Result<Client> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Client::builder()
        // A receiver can refuse a request without one, and the client sends none by default.
        .user_agent(concat!("engine-alerts/", env!("CARGO_PKG_VERSION")))
        .tls_backend_preconfigured(tls)
        .timeout(REQUEST_TIMEOUT)
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .context("building the webhook client")
}

/// Posts committed pages in route order, retaining them if the worker is cancelled.
/// Error cases:
/// - an outbox read or acknowledgement fails
pub(super) async fn deliver(
    mut wake: mpsc::Receiver<()>,
    outbox: Outbox,
    severity: Severity,
    client: Client,
    url: WebhookUrl,
    metrics: Arc<Metrics>,
    retry: Retry,
) -> anyhow::Result<()> {
    loop {
        let Some(pending) = outbox.next(severity).await? else {
            if wake.recv().await.is_none() {
                return Ok(());
            }
            continue;
        };
        let page = &pending.page;
        let failed =
            metrics.alert_attempts_failed.get_or_create(&SeverityLabel { severity: severity.as_str() }).clone();
        let remaining = pending.remaining();
        let result = if remaining.is_zero() {
            Err(anyhow!("page expired in the durable outbox"))
        } else {
            let retry = Retry { give_up_after: remaining.min(retry.give_up_after), ..retry };
            timeout(remaining, post(&client, &url, page, retry, &failed))
                .await
                .unwrap_or_else(|_| Err(anyhow!("page expired while posting")))
        };
        let outcome = match result {
            Ok(()) => {
                info!(key = %page.key, status = ?page.status, context = %page.context, "alert page sent");
                "sent"
            }
            Err(e) => {
                error!(key = %page.key, status = ?page.status, context = %page.context, "alert page not delivered: {e:#}");
                "failed"
            }
        };
        // Acceptance followed by a crash before this commit can redeliver the same episode key.
        outbox.remove(pending.id).await.inspect_err(|e| {
            error!(key = %page.key, context = %page.context, "alert page acknowledgement failed: {e:#}");
        })?;
        metrics.alert_deliveries.get_or_create(&DeliveryLabels { severity: severity.as_str(), outcome }).inc();
    }
}

/// POSTs one page until a 2xx answer, retrying per `retry` when the failure is one retrying can
/// fix: no connection, a deadline, 408, 429, or 5xx. Each failed attempt counts on `failed` as it
/// happens, so an outage shows while the page is still retrying. Errors never carry the URL.
/// Error cases:
/// - the webhook answered a status retrying cannot change: 3xx (redirects are not followed) or 4xx
/// - failures retrying can fix lasted `retry.give_up_after`
pub(super) async fn post(
    client: &Client,
    url: &WebhookUrl,
    page: &Page,
    retry: Retry,
    failed: &Counter,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut wait = retry.first;
    let mut attempt = 1;
    loop {
        let err = match client.post(url.expose().clone()).json(page).send().await {
            Ok(answer) if answer.status().is_success() => return Ok(()),
            Ok(answer) if !retryable(answer.status()) => {
                failed.inc();
                bail!("webhook refused the page: {}", answer.status());
            }
            Ok(answer) => anyhow!("webhook answered {}", answer.status()),
            Err(e) => anyhow!(e.without_url()),
        };
        failed.inc();
        if started.elapsed() + wait > retry.give_up_after {
            return Err(err).context(format!("gave up after {attempt} attempts"));
        }
        if attempt == 1 {
            warn!(key = %page.key, context = %page.context, "alert page not delivered yet, retrying: {err:#}");
        } else {
            debug!(key = %page.key, context = %page.context, attempt, "alert page attempt failed: {err:#}");
        }
        sleep(wait).await;
        wait = (wait * 2).min(retry.max);
        attempt += 1;
    }
}

/// Whether a failed answer may succeed when sent again.
fn retryable(status: StatusCode) -> bool {
    status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT || status == StatusCode::TOO_MANY_REQUESTS
}
