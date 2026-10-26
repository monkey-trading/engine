//! The key-distribution service stand-ins: one shaped like the vendor's endpoints, and counted
//! failure variants for the retry tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::Path;
use axum::routing::get;
use futures_util::StreamExt as _;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::super::Options;
use rcgen::SerialNumber;

use super::fixtures::{PROCESSOR, TestChain, pem};

/// A key-distribution service stand-in with the vendor endpoints' shapes: the VCEK endpoint
/// answers for any chip and TCB (so the verifier itself must bind the certificate's signed
/// extensions to the quote), the issuer chain and revocation list answer PEM, and all only for
/// the fixture's processor. `attempts` counts answers on the counting stand-ins, for the retry
/// tests.
pub(super) struct KdsStub {
    pub(super) base: String,
    pub(super) attempts: Arc<AtomicUsize>,
    _task: JoinHandle<()>,
}

pub(super) async fn kds_stub(chain: &TestChain) -> KdsStub {
    kds_stub_with(chain, false, &[]).await
}

/// The stand-in with a PEM-encoded VCEK or a revocation list that revokes `revoked`.
pub(super) async fn kds_stub_with(chain: &TestChain, vcek_pem: bool, revoked: &[SerialNumber]) -> KdsStub {
    let vcek = if vcek_pem { pem(&chain.vcek).into_bytes() } else { chain.vcek.der().to_vec() };
    let crl = super::fixtures::crl_revoking(chain, revoked);
    let app = Router::new()
        .route("/vcek/v1/{processor}/cert_chain", {
            let ark = pem(&chain.ark);
            let ask = pem(&chain.ask);
            get(move |Path(processor): Path<String>| {
                let ark = ark.clone();
                let ask = ask.clone();
                async move {
                    if processor != PROCESSOR {
                        return (axum::http::StatusCode::NOT_FOUND, String::new());
                    }
                    (axum::http::StatusCode::OK, format!("{ask}{ark}"))
                }
            })
        })
        .route("/vcek/v1/{processor}/crl", {
            let crl = crl.clone();
            get(move |Path(processor): Path<String>| {
                let crl = crl.clone();
                async move {
                    if processor != PROCESSOR {
                        return (axum::http::StatusCode::NOT_FOUND, String::new());
                    }
                    (axum::http::StatusCode::OK, crl)
                }
            })
        })
        .route("/vcek/v1/{processor}/{hwid}", {
            let vcek = vcek.clone();
            get(move |Path((processor, _hwid)): Path<(String, String)>| {
                let vcek = vcek.clone();
                async move {
                    // The vendor answers DER, and a mirror answers whatever it holds; the
                    // verifier must bind the certificate's extensions to the quote itself.
                    if processor != PROCESSOR {
                        return (axum::http::StatusCode::NOT_FOUND, Vec::new());
                    }
                    (axum::http::StatusCode::OK, vcek)
                }
            })
        });
    serve(app).await
}

/// A stand-in whose one route answers `status` `failures` times, counted, then `body`.
pub(super) async fn status_stub(status: axum::http::StatusCode, failures: usize, body: String) -> KdsStub {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = attempts.clone();
    let app = Router::new().route(
        "/vcek/v1/{processor}/cert_chain",
        get(move |_path: Path<String>| {
            let counted = counted.clone();
            let body = body.clone();
            async move {
                let attempt = counted.fetch_add(1, Ordering::SeqCst);
                if attempt < failures { (status, String::new()) } else { (axum::http::StatusCode::OK, body) }
            }
        }),
    );
    let stub = serve(app).await;
    KdsStub { base: stub.base, attempts, _task: stub._task }
}

/// A stand-in whose one route answers a 4 MiB chunked body with no `Content-Length`, counted.
pub(super) async fn streamed_oversized_stub() -> KdsStub {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = attempts.clone();
    let app = Router::new().route(
        "/vcek/v1/{processor}/cert_chain",
        get(move |_path: Path<String>| {
            let counted = counted.clone();
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                let chunk = "x".repeat(64 * 1024);
                let stream = futures_util::stream::repeat(Ok::<_, std::convert::Infallible>(chunk)).take(64);
                axum::body::Body::from_stream(stream)
            }
        }),
    );
    let stub = serve(app).await;
    KdsStub { base: stub.base, attempts, _task: stub._task }
}

async fn serve(app: Router) -> KdsStub {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binding the stub");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let _task = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serving the stub");
    });
    KdsStub { base, attempts: Arc::new(AtomicUsize::new(0)), _task }
}

pub(super) fn options<'a>(stub: &'a KdsStub, root: &'a str) -> Options<'a> {
    Options { processor: PROCESSOR, kds_base: &stub.base, root_pem: Some(root) }
}
