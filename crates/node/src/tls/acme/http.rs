//! The ACME client's HTTP: the node's own client, so the CA is authenticated against roots compiled
//! into the binary, never the host's trust store.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper::http::Response;
use instant_acme::{BodyWrapper, BytesResponse, HttpClient};
use reqwest::{Client, Url, redirect};
use rustls::{ClientConfig, RootCertStore};

use crate::config::AcmeTestCa;

/// Every request to the CA, from connecting to its body's last byte.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest response body taken from the CA; a certificate chain is a few kilobytes.
pub(super) const MAX_RESPONSE_BYTES: usize = 1 << 20;

/// Requests to the CA over TLS that must chain to the compiled-in Mozilla roots, or to a test CA's
/// root alone. The host resolves the CA's name and carries the connection, so it can delay or drop
/// requests, but not answer them without a certificate for the CA's name. No proxy, no redirect,
/// and a deadline on each.
///
/// TODO: certificate validity is judged by the untrusted host clock and revocation is not checked,
/// so a host holding an expired or revoked key for the CA's name could set the clock back and pose
/// as the CA, which gains it no more than delaying issuance; a trusted time bound closes the first.
#[derive(Clone)]
pub(crate) struct AcmeHttp(Client);

impl AcmeHttp {
    /// A client trusting the compiled-in roots, or `test_ca`'s root alone.
    /// Error cases:
    /// - the test CA's root does not parse, or the TLS or HTTP client cannot be built
    pub(super) fn new(test_ca: Option<&AcmeTestCa>) -> anyhow::Result<Self> {
        let roots = match test_ca {
            Some(test_ca) => {
                let mut roots = RootCertStore::empty();
                roots.add(test_ca.root_certificate()?).context("api.tls.acme.test_ca.root")?;
                roots
            }
            None => RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        };
        let tls = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .context("TLS protocol versions")?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let client = Client::builder()
            .user_agent(concat!("engine-node/", env!("CARGO_PKG_VERSION")))
            .tls_backend_preconfigured(tls)
            .timeout(REQUEST_TIMEOUT)
            .redirect(redirect::Policy::none())
            .no_proxy()
            .build()
            .context("building the ACME client")?;
        Ok(Self(client))
    }
}

impl HttpClient for AcmeHttp {
    /// Sends `request` and reads the whole response.
    fn request(
        &self,
        request: Request<BodyWrapper<Bytes>>,
    ) -> Pin<Box<dyn Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>> {
        let client = self.0.clone();
        Box::pin(async move { send(&client, request).await.map_err(|e| instant_acme::Error::Other(e.into())) })
    }
}

/// Sends `request` and reads the response, up to `MAX_RESPONSE_BYTES` of body.
/// Error cases:
/// - the URL is not absolute, the request fails or times out, or the body is too large
async fn send(client: &Client, request: Request<BodyWrapper<Bytes>>) -> anyhow::Result<BytesResponse> {
    let (parts, body) = request.into_parts();
    let url = Url::parse(&parts.uri.to_string()).context("ACME request URL")?;
    let body = body.collect().await.map(|collected| collected.to_bytes()).unwrap_or_else(|never| match never {});
    let mut response = client
        .request(parts.method, url.clone())
        .headers(parts.headers)
        .body(body)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    let mut builder = Response::builder().status(response.status());
    if let Some(headers) = builder.headers_mut() {
        headers.extend(response.headers().clone());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.with_context(|| format!("reading {url}"))? {
        ensure!(bytes.len() + chunk.len() <= MAX_RESPONSE_BYTES, "{url} answered more than {MAX_RESPONSE_BYTES} bytes");
        bytes.extend_from_slice(&chunk);
    }
    let response = builder.body(Full::new(Bytes::from(bytes))).context("ACME response")?;
    Ok(BytesResponse::from(response))
}
