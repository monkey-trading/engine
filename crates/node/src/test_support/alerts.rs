use std::fmt::Debug;

use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::subscriber::DefaultGuard;
use tracing::{Event as LogEvent, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

use crate::test_support::Alert;
use crate::test_tracing::keep_callsites_interested;

/// Collects the fields of every event that carries the `key` field: `alert`, or `alert_resolved`.
struct AlertLayer(mpsc::UnboundedSender<Alert>, &'static str);

/// Records an event's fields as strings.
#[derive(Default)]
struct Fields(Alert);

impl<S: Subscriber> Layer<S> for AlertLayer {
    fn on_event(&self, event: &LogEvent<'_>, _: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.0.contains_key(self.1) {
            fields.0.insert("level".to_owned(), event.metadata().level().to_string());
            let _ = self.0.send(fields.0);
        }
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

/// Routes this thread's logs to an alert collector until the guard drops. `#[tokio::test]` runs
/// every task on the test's thread, so the alerts of the tasks a test spawns all arrive.
pub fn capture_alerts() -> (DefaultGuard, mpsc::UnboundedReceiver<Alert>) {
    keep_callsites_interested();
    let (tx, rx) = mpsc::unbounded_channel();
    (tracing::subscriber::set_default(Registry::default().with(AlertLayer(tx, "alert"))), rx)
}

/// As `capture_alerts`, and the events that end an alert's episode (those with an `alert_resolved`
/// field) arrive on the second receiver.
pub fn capture_alerts_and_resolutions() -> (DefaultGuard, mpsc::UnboundedReceiver<Alert>, mpsc::UnboundedReceiver<Alert>)
{
    keep_callsites_interested();
    let (alerts_tx, alerts) = mpsc::unbounded_channel();
    let (resolved_tx, resolved) = mpsc::unbounded_channel();
    let subscriber =
        Registry::default().with(AlertLayer(alerts_tx, "alert")).with(AlertLayer(resolved_tx, "alert_resolved"));
    (tracing::subscriber::set_default(subscriber), alerts, resolved)
}
