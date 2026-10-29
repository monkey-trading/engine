//! The ACME client's HTTP against a loopback HTTPS stand-in for the CA: which roots it trusts, and
//! the bounds on what it takes back.

use std::sync::Arc;
use std::time::Duration;

use hyper::Request;
use hyper::body::Bytes;
use instant_acme::{BodyWrapper, BytesResponse, HttpClient};
use rcgen::{CertificateParams, DnType, IsCa, Issuer, KeyPair};
use rustls::ServerConfig;
use rustls::pki_types::PrivatePkcs8KeyDer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::config::AcmeTestCa;
use crate::tls::acme::http::{AcmeHttp, MAX_RESPONSE_BYTES, REQUEST_TIMEOUT};

#[tokio::test]
async fn a_body_up_to_the_cap_is_read_and_one_byte_more_is_refused() {
    for (len, taken) in [(MAX_RESPONSE_BYTES, true), (MAX_RESPONSE_BYTES + 1, false)] {
        let (port, root) = stand_in(ok(len)).await;
        let read = request(&trusting(&root), port).await;
        match read {
            Ok(mut response) => {
                assert!(taken, "a {len}-byte body was read");
                assert_eq!(response.body.into_bytes().await.unwrap().len(), len);
            }
            Err(e) => {
                assert!(!taken, "a {len}-byte body was refused: {e}");
                assert!(e.to_string().contains("answered more than"), "{e}");
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_request_the_ca_never_finishes_answering_fails_at_the_request_deadline() {
    // Nothing answered, and a body that stops short of the length its headers promise.
    let stalled_body = b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nx".to_vec();
    for (case, sent) in [("no answer", Vec::new()), ("a stalled body", stalled_body)] {
        let (port, root) = stalled(sent).await;
        let started = tokio::time::Instant::now();
        let client = trusting(&root);
        let giving_up = tokio::time::timeout(REQUEST_TIMEOUT * 2, request(&client, port));
        let err = giving_up.await.unwrap_or_else(|_| panic!("{case}: never gave up")).err().expect(case);
        assert!(format!("{err:?}").contains("timed out"), "{case}: {err:?}");
        let waited = started.elapsed();
        assert!(waited >= REQUEST_TIMEOUT && waited < REQUEST_TIMEOUT + Duration::from_secs(1), "{case}: {waited:?}");
    }
}

#[tokio::test]
async fn a_redirect_is_handed_back_unfollowed() {
    let (port, root) = stand_in(
        b"HTTP/1.1 302 Found\r\nlocation: https://127.0.0.1:1/elsewhere\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            .to_vec(),
    )
    .await;
    let response = request(&trusting(&root), port).await.unwrap();
    assert_eq!(response.parts.status, 302, "not followed to the unreachable port it names");
}

#[tokio::test]
async fn the_compiled_in_roots_refuse_a_ca_that_only_a_test_root_certifies() {
    let (port, root) = stand_in(ok(2)).await;
    request(&trusting(&root), port).await.unwrap();
    let err = request(&AcmeHttp::new(None).unwrap(), port).await.err().expect("refused without the test root");
    assert!(format!("{err:?}").to_lowercase().contains("certificate"), "{err:?}");
}

// ── Helpers ──

/// A client trusting `root` alone.
fn trusting(root: &str) -> AcmeHttp {
    AcmeHttp::new(Some(&AcmeTestCa { root: root.to_owned() })).unwrap()
}

/// `GET https://127.0.0.1:<port>/dir` through `client`.
async fn request(client: &AcmeHttp, port: u16) -> Result<BytesResponse, instant_acme::Error> {
    let request = Request::builder().uri(format!("https://127.0.0.1:{port}/dir")).body(BodyWrapper::<Bytes>::default());
    client.request(request.unwrap()).await
}

/// A 200 response with a body of `len` bytes.
fn ok(len: usize) -> Vec<u8> {
    let mut response = format!("HTTP/1.1 200 OK\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n").into_bytes();
    response.resize(response.len() + len, b'x');
    response
}

/// A stand-in on a free loopback port that answers each request with `response`, certified for
/// 127.0.0.1 by a fresh root, which it returns as PEM.
async fn stand_in(response: Vec<u8>) -> (u16, String) {
    serve(response, false).await
}

/// A stand-in that sends `sent` (possibly nothing) to each request, then holds the connection open
/// without finishing.
async fn stalled(sent: Vec<u8>) -> (u16, String) {
    serve(sent, true).await
}

async fn serve(response: Vec<u8>, stall: bool) -> (u16, String) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.distinguished_name.push(DnType::CommonName, "stand-in CA");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let root = ca.self_signed(&ca_key).unwrap();
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["127.0.0.1".to_owned()]).unwrap();
    let leaf = leaf.signed_by(&key, &Issuer::new(ca, &ca_key)).unwrap();
    let config = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![leaf.der().clone()], PrivatePkcs8KeyDer::from(key.serialize_der()).into())
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (acceptor, response) = (acceptor.clone(), response.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    match tls.read(&mut buf).await {
                        Ok(n @ 1..) => request.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                }
                let _ = tls.write_all(&response).await;
                if stall {
                    let _ = tls.flush().await;
                    std::future::pending::<()>().await;
                }
                let _ = tls.shutdown().await;
            });
        }
    });
    (port, root.pem())
}
