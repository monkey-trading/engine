//! The key-distribution service stand-ins: one shaped like the vendor's endpoints, one that
//! fails a counted number of times for the retry tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::Path;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::super::Options;
use super::fixtures::{PROCESSOR, TestChain, pem};

/// A key-distribution service stand-in with the vendor endpoints' shapes: the VCEK endpoint
/// answers DER for any chip and TCB, like a mirror would, so the verifier's own extension
/// binding is what refuses a mismatched certificate; the issuer chain answers PEM, and only for
/// the fixture's processor. `failures` counts 500 answers before `body` succeeds on one route,
/// for the retry tests.
pub(super) struct KdsStub {
    pub(super) base: String,
    pub(super) attempts: Arc<AtomicUsize>,
    _task: JoinHandle<()>,
}

pub(super) async fn kds_stub(chain: &TestChain) -> KdsStub {
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
        .route("/vcek/v1/{processor}/{hwid}", {
            let vcek = chain.vcek.der().to_vec();
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

/// A stand-in whose one route fails with 500 `failures` times, counted, before answering `body`.
pub(super) async fn flaky_stub(failures: usize, body: String) -> KdsStub {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = attempts.clone();
    let app = Router::new().route(
        "/vcek/v1/{processor}/cert_chain",
        get(move |_path: Path<String>| {
            let counted = counted.clone();
            let body = body.clone();
            async move {
                let attempt = counted.fetch_add(1, Ordering::SeqCst);
                if attempt < failures {
                    (axum::http::StatusCode::INTERNAL_SERVER_ERROR, String::new())
                } else {
                    (axum::http::StatusCode::OK, body)
                }
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
