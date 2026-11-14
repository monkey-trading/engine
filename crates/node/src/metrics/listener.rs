//! The operator's metrics listener, bound once for the process and served on its own task beside
//! everything it runs, so it answers while a member or a standby follows, a replica's build
//! included, as well as while a node sequences.

use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tracing::info;

use super::{Metrics, router};

/// The metrics listener on `listen`.
/// Error cases:
/// - `listen` cannot be bound
pub(crate) async fn bind(listen: SocketAddr) -> anyhow::Result<TcpListener> {
    let listener = TcpListener::bind(listen).await.with_context(|| format!("binding {listen}"))?;
    info!(listen = %listener.local_addr()?, "metrics up");
    Ok(listener)
}

/// Runs `run` while `listener` serves `metrics` on a task of its own, off `run`'s future, which a
/// member's replica build holds while it replays: scrapes answer through it. Ends with `run`.
/// Error cases:
/// - `run` fails, or the metrics server stops before it ends
pub(crate) async fn alongside<T>(
    listener: TcpListener,
    metrics: Arc<Metrics>,
    run: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let mut serving = JoinSet::new();
    serving.spawn(axum::serve(listener, router(metrics)).into_future());
    tokio::select! {
        outcome = run => outcome,
        Some(stopped) = serving.join_next() => Err(anyhow!("the metrics server stopped: {stopped:?}")),
    }
}
