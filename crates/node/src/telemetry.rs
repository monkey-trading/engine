//! Logging, metrics, and alert routing, set up before the node runs: the global log subscriber,
//! which feeds alert lines to the router whatever the log level and holds the HTTP client's own
//! logs at `warn`, and the node's metrics registry.

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::Context;
use tracing::{Level, Subscriber};
use tracing_subscriber::filter::{FilterExt, filter_fn};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt, registry};

use crate::alerts::{self, AlertLayer, Routes, Severity, is_alert_callsite};
use crate::config::{NodeConfig, WebhookRoute};
use crate::metrics::Metrics;

/// The HTTP client's crates, whose logs stay at `warn` or above whatever `logging.level` says.
const HTTP_CLIENT_TARGETS: [&str; 3] = ["reqwest", "hyper", "hyper_util"];

/// What `run` needs that exists before logging starts: the node's metrics, and the alert router the
/// log subscriber feeds.
pub struct Telemetry {
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) alerts: alerts::Router,
    pub(crate) pages_critical: bool,
}

/// Installs the global log subscriber at `level`, a `tracing` filter directive. Colour codes
/// go only to a terminal, so a log file or collector gets plain `field=value` text.
/// Error cases:
/// - `level` is not a valid filter directive
pub fn init_logging(level: &str) -> anyhow::Result<()> {
    install_subscriber(level, None)
}

/// Reads the alert webhooks from the environment through `env`, builds the node's metrics, and
/// installs the global log subscriber at `cfg.logging.level` with alert routing.
/// Error cases:
/// - a configured webhook's variable is unset, or does not hold an acceptable URL
/// - `logging.level` is not a valid filter directive
pub fn init_telemetry(cfg: &NodeConfig, env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Telemetry> {
    let routes = Routes::from_config(&cfg.alerts, env)?;
    let pages_critical = routes.has(Severity::Critical);
    let metrics = Arc::new(Metrics::new());
    let (layer, alerts) = alerts::channel(routes, metrics.clone());
    install_subscriber(&cfg.logging.level, Some(layer))?;
    Ok(Telemetry { metrics, alerts, pages_critical })
}

/// As `init_telemetry`, for a worker with only the alert routes and a log level of its own.
/// Error cases:
/// - a configured webhook's variable is unset, or does not hold an acceptable URL
/// - `level` is not a valid filter directive
pub(crate) fn init_worker_telemetry(
    level: &str,
    critical: &Option<WebhookRoute>,
    warning: &Option<WebhookRoute>,
    env: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<Telemetry> {
    let routes = Routes::from_routes(critical, warning, env)?;
    let pages_critical = routes.has(Severity::Critical);
    let metrics = Arc::new(Metrics::new());
    let (layer, alerts) = alerts::channel(routes, metrics.clone());
    install_subscriber(level, Some(layer))?;
    Ok(Telemetry { metrics, alerts, pages_critical })
}

/// Installs the global log subscriber on stdout, with colour codes only when it is a terminal, so a
/// log file or collector gets plain `field=value` text; see `log_subscriber`.
/// Error cases:
/// - `level` is not a valid filter directive
fn install_subscriber(level: &str, alerts: Option<AlertLayer>) -> anyhow::Result<()> {
    log_subscriber(level, alerts, std::io::stdout, std::io::stdout().is_terminal())?.init();
    Ok(())
}

/// The log formatter at `level`, writing to `writer` with colour codes if `ansi`, plus `alerts` when
/// given, which sees alert lines whatever the level. The HTTP client's own logs stay at `warn` or
/// above, since a request's URL can be a credential.
/// Error cases:
/// - `level` is not a valid filter directive
fn log_subscriber<W>(
    level: &str,
    alerts: Option<AlertLayer>,
    writer: W,
    ansi: bool,
) -> anyhow::Result<impl Subscriber + Send + Sync>
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let filter = EnvFilter::try_new(level).context("log level")?;
    // Applied on top of `filter`, so no directive in `logging.level` can lift it.
    let quiet_http_client = filter_fn(|meta| *meta.level() <= Level::WARN || !is_http_client(meta.target()));
    Ok(registry()
        .with(fmt::layer().with_ansi(ansi).with_writer(writer).with_filter(filter.and(quiet_http_client)))
        .with(alerts.map(|layer| layer.with_filter(filter_fn(is_alert_callsite)))))
}

/// Whether `target` is in one of the HTTP client's crates.
fn is_http_client(target: &str) -> bool {
    HTTP_CLIENT_TARGETS
        .iter()
        .any(|crate_name| target.strip_prefix(crate_name).is_some_and(|rest| rest.is_empty() || rest.starts_with("::")))
}

#[cfg(test)]
mod tests {
    use super::*;

    use tracing::subscriber::with_default;
    use tracing::{debug, info, info_span, trace, warn};

    use crate::alerts::AlertLine;
    use crate::test_support::Written;

    #[test]
    fn http_client_logs_stay_quiet_whatever_the_log_level() {
        for level in ["trace", "info,reqwest::connect=debug,hyper_util=trace"] {
            let written = Written::default();
            let subscriber = log_subscriber(level, None, written.clone(), false).unwrap();
            with_default(subscriber, || {
                debug!(target: "reqwest::connect", "posting to https://hooks.example/secret-token");
                trace!(target: "hyper_util::client", "request to https://hooks.example/secret-token");
                warn!(target: "reqwest::connect", "client warning");
                info!(target: "engine_node::node", "node detail");
                info!(target: "reqwestful", "another crate");
            });
            let text = written.text();
            assert!(!text.contains("secret-token"), "{level}: {text}");
            for kept in ["client warning", "node detail", "another crate"] {
                assert!(text.contains(kept), "{level}: {text}");
            }
        }
    }

    #[test]
    fn alert_lines_route_with_their_span_fields_whatever_the_log_level() {
        let (layer, router) = alerts::channel(Routes::default(), Arc::new(Metrics::new()));
        let subscriber = log_subscriber("error", Some(layer), std::io::sink, false).unwrap();
        with_default(subscriber, || {
            let _span = info_span!("command", id = 42).entered();
            warn!(alert = "prices_rejected", market = 1, "price update refused");
        });
        let [line]: [AlertLine; 1] = router.queued().try_into().unwrap();
        assert_eq!(line.name, "prices_rejected");
        assert_eq!(line.context, "command{id=42}");
    }
}
