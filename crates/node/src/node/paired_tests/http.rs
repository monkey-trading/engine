//! The actual public router over a proof-verified reader, with a supervised test-only listener.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use engine_core::auth::ClientAuth;
use engine_core::protocol::ProtocolVersion;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::api::{ApiState, MAX_BACKFILLS, router};
use crate::clock::SystemClock;
use crate::history::Reader;
use crate::limits::ApiLimits;
use crate::log::Manifest;
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::dev_config;

pub(crate) struct Http {
    addr: SocketAddr,
    client: Option<reqwest::Client>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Http {
    /// Serves history through the unchanged validation, error mapping and response serialization.
    pub(crate) async fn start(history: Reader) -> Self {
        let mut cfg = dev_config();
        cfg.api.per_ip.per_second = 1_000_000.try_into().unwrap();
        cfg.api.per_ip.burst = 1_000_000.try_into().unwrap();
        let state = ApiState {
            tx: mpsc::channel(1).0,
            stream: broadcast::channel(1).0,
            attestation: Arc::new(
                attest_bundle(
                    &MockAttestor,
                    &AttestationBundle {
                        enclave_address: EnclaveKey::test_key().address(),
                        config_sha256: [0; 32],
                        tls_public_key_sha256: [0; 32],
                    },
                )
                .unwrap(),
            ),
            auth: ClientAuth::Unsigned,
            operator: cfg.operator().unwrap(),
            mock_custody: false,
            payouts_enabled: false,
            clock: Arc::new(SystemClock),
            log_dir: PathBuf::new(),
            manifest: watch::channel(Manifest::default()).1,
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            limits: Arc::new(ApiLimits::new(&cfg.api)),
            history,
            marks: crate::test_support::marks_reader(),
            emission_budget: None,
            collateral: None,
            listing: Default::default(),
            signing: crate::test_support::signing(),
            protocol: ProtocolVersion::LATEST,
            access: None,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state);
        let (shutdown, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Self {
            addr,
            client: Some(reqwest::Client::builder().timeout(crate::test_support::LOADED_WAIT).build().unwrap()),
            shutdown: Some(shutdown),
            task: Some(task),
        }
    }

    /// Closes the client's connections and waits until the server has dropped every handler, so
    /// no reader it served keeps a history store open across a restart.
    pub(crate) async fn stop(mut self) {
        self.client.take();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.await.unwrap();
        }
    }

    /// Returns the status and the JSON body, or a refusal's plain-text body as a JSON string.
    pub(crate) async fn get(&self, path: &str) -> Result<(u16, Value)> {
        let client = self.client.as_ref().expect("a running test server");
        let response = client.get(format!("http://{}{path}", self.addr)).send().await?;
        let status = response.status().as_u16();
        let body = response.text().await?;
        Ok((status, serde_json::from_str(&body).unwrap_or(Value::String(body))))
    }
}

impl Drop for Http {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
