//! A scripted ACME CA behind the client's `HttpClient`: it answers what the certificate task sends,
//! without checking signatures, validates when told the challenge is ready, issues for the key in
//! the finalize request, and drops, stalls, or answers with a problem the exchanges a test asks it to.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper::http::Response;
use instant_acme::{BodyWrapper, BytesResponse, HttpClient};
use rcgen::{CertificateParams, DnType, IsCa, Issuer, KeyPair, PublicKeyData, SignatureAlgorithm, date_time_ymd};
use serde_json::{Value, json};
use x509_parser::certification_request::X509CertificationRequest;
use x509_parser::prelude::FromDer;

const BASE: &str = "https://ca.test";

/// The CA, shared between the test and the client it hands the task.
#[derive(Clone, Default)]
pub(super) struct FakeCa(Arc<Mutex<State>>);

/// What a test scripts and reads back.
#[derive(Default)]
pub(super) struct State {
    /// The domain each issued certificate names.
    pub(super) names: String,
    /// Whether the account's validation of the domain is current, so a new order is ready at once.
    pub(super) authorized: bool,
    /// Whether the next validation fails, recording why on the challenge.
    pub(super) refuse_validation: bool,
    /// Whether a validation the challenge starts never ends, so the order stays pending.
    pub(super) never_validate: bool,
    /// How many finalize answers, and certificate downloads, to drop after the CA acts on them.
    pub(super) lose_finalize: usize,
    pub(super) lose_downloads: usize,
    /// Problem documents, as status and type, answering the next order reads in turn, `None` for
    /// one answered as usual, and the next downloads.
    pub(super) order_problems: Vec<Option<(u16, &'static str)>>,
    pub(super) download_problems: Vec<(u16, &'static str)>,
    /// Whether the next new order is refused for an account the CA no longer knows.
    pub(super) forget_account: bool,
    /// An endpoint whose next request the CA acts on and then never answers.
    pub(super) stall: Option<&'static str>,
    /// Requests by endpoint: `new-account`, `new-order`, `challenge`, `finalize`, `certificate`.
    pub(super) requests: BTreeMap<&'static str, usize>,
    orders: Vec<FakeOrder>,
}

/// One order: its status and, once issued, its chain.
struct FakeOrder {
    status: &'static str,
    chain: Option<String>,
}

/// The public key a certificate request carries, as rcgen signs for it.
struct RequestedKey(Vec<u8>);

impl PublicKeyData for RequestedKey {
    /// The key's encoded point.
    fn der_bytes(&self) -> &[u8] {
        &self.0
    }

    /// ECDSA P-256, the API key's algorithm.
    fn algorithm(&self) -> &'static SignatureAlgorithm {
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

impl HttpClient for FakeCa {
    /// Answers `request` as the CA would, or fails it as the test scripted.
    fn request(
        &self,
        request: Request<BodyWrapper<Bytes>>,
    ) -> Pin<Box<dyn Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>> {
        let ca = self.clone();
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let body =
                body.collect().await.map(|collected| collected.to_bytes()).unwrap_or_else(|never| match never {});
            let path = parts.uri.path().to_owned();
            match ca.answer(&path, &body) {
                Ok(Some((status, location, json))) => Ok(reply(status, location, json)),
                Ok(None) => std::future::pending().await,
                Err(e) => Err(e),
            }
        })
    }
}

impl FakeCa {
    /// A CA issuing for `names`.
    pub(super) fn new(names: &str) -> Self {
        let ca = Self::default();
        ca.state().names = names.to_owned();
        ca
    }

    /// The script and its record.
    pub(super) fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    /// How many requests reached `endpoint`.
    pub(super) fn count(&self, endpoint: &str) -> usize {
        self.state().requests.get(endpoint).copied().unwrap_or(0)
    }

    /// The status, `Location`, and body answering a request for `path` carrying `body`; `None`
    /// for a request the CA never answers.
    fn answer(&self, path: &str, body: &[u8]) -> Result<Option<(u16, Option<String>, Value)>, instant_acme::Error> {
        let answered = self.act(path, body)?;
        let mut state = self.state();
        let endpoint = path.trim_start_matches('/').split('/').next().unwrap_or_default();
        Ok(if state.stall == Some(endpoint) {
            state.stall = None;
            None
        } else {
            Some(answered)
        })
    }

    /// Does what a request for `path` carrying `body` asks, and the answer it would get.
    fn act(&self, path: &str, body: &[u8]) -> Result<(u16, Option<String>, Value), instant_acme::Error> {
        let mut state = self.state();
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let id = |segments: &[&str]| segments.get(1).and_then(|id| id.parse::<usize>().ok()).unwrap_or(0);
        Ok(match segments[0] {
            // The staging and production directories of one CA.
            "dir" | "production" => (
                200,
                None,
                json!({"newNonce": format!("{BASE}/nonce"), "newAccount": format!("{BASE}/new-account"),
                       "newOrder": format!("{BASE}/new-order")}),
            ),
            "nonce" => (200, None, Value::Null),
            "new-account" => {
                *state.requests.entry("new-account").or_default() += 1;
                (201, Some(format!("{BASE}/account/1")), json!({"status": "valid"}))
            }
            "new-order" if state.forget_account => {
                state.forget_account = false;
                problem(400, "accountDoesNotExist")
            }
            "new-order" => {
                *state.requests.entry("new-order").or_default() += 1;
                let status = if state.authorized { "ready" } else { "pending" };
                state.orders.push(FakeOrder { status, chain: None });
                let id = state.orders.len() - 1;
                (201, Some(format!("{BASE}/order/{id}")), order_json(id, &state.orders[id]))
            }
            "order" if state.order_problems.first().is_some_and(Option::is_some) => {
                let (status, kind) = state.order_problems.remove(0).expect("checked above");
                problem(status, kind)
            }
            "order" if !state.order_problems.is_empty() => {
                state.order_problems.remove(0);
                (200, None, order_json(id(&segments), &state.orders[id(&segments)]))
            }
            "order" => (200, None, order_json(id(&segments), &state.orders[id(&segments)])),
            "authz" => {
                let id = id(&segments);
                let (authz, challenge, error) = match state.orders[id].status {
                    "invalid" => (
                        "invalid",
                        "invalid",
                        json!({"type": "urn:ietf:params:acme:error:connection",
                                                               "detail": "connection refused"}),
                    ),
                    _ if state.authorized => ("valid", "valid", Value::Null),
                    _ => ("pending", "pending", Value::Null),
                };
                let names = state.names.clone();
                let challenge = json!({"type": "tls-alpn-01", "url": format!("{BASE}/challenge/{id}"),
                                       "token": "token", "status": challenge, "error": error});
                (
                    200,
                    None,
                    json!({"identifier": {"type": "dns", "value": names}, "status": authz,
                                   "challenges": [challenge]}),
                )
            }
            "challenge" => {
                *state.requests.entry("challenge").or_default() += 1;
                let id = id(&segments);
                if state.refuse_validation {
                    state.orders[id].status = "invalid";
                } else if !state.never_validate {
                    state.authorized = true;
                    state.orders[id].status = "ready";
                }
                (
                    200,
                    None,
                    json!({"type": "tls-alpn-01", "url": format!("{BASE}/challenge/{id}"), "token": "token",
                                   "status": "processing"}),
                )
            }
            "finalize" => {
                *state.requests.entry("finalize").or_default() += 1;
                let id = id(&segments);
                let chain = issue(&state.names, &finalize_request(body));
                state.orders[id] = FakeOrder { status: "valid", chain: Some(chain) };
                if state.lose_finalize > 0 {
                    state.lose_finalize -= 1;
                    return Err(lost());
                }
                (200, None, order_json(id, &state.orders[id]))
            }
            "certificate" => {
                *state.requests.entry("certificate").or_default() += 1;
                if !state.download_problems.is_empty() {
                    let (status, kind) = state.download_problems.remove(0);
                    return Ok(problem(status, kind));
                }
                if state.lose_downloads > 0 {
                    state.lose_downloads -= 1;
                    return Err(lost());
                }
                let chain = state.orders[id(&segments)].chain.clone().expect("downloaded only once issued");
                (200, None, Value::String(chain))
            }
            other => panic!("the certificate task requested an unknown endpoint {other}"),
        })
    }
}

/// A problem document answer of `status` and ACME error type `kind`.
fn problem(status: u16, kind: &str) -> (u16, Option<String>, Value) {
    let json = json!({"type": format!("urn:ietf:params:acme:error:{kind}"), "detail": kind, "status": status});
    (status, None, json)
}

/// An order's JSON.
fn order_json(id: usize, order: &FakeOrder) -> Value {
    let mut json = json!({"status": order.status, "authorizations": [format!("{BASE}/authz/{id}")],
                          "finalize": format!("{BASE}/finalize/{id}")});
    if order.chain.is_some() {
        json["certificate"] = format!("{BASE}/certificate/{id}").into();
    }
    json
}

/// A response with a fresh nonce: a JSON body, or the bare text of a string body.
fn reply(status: u16, location: Option<String>, json: Value) -> BytesResponse {
    let mut response = Response::builder().status(status).header("Replay-Nonce", "nonce");
    if let Some(location) = location {
        response = response.header("Location", location);
    }
    let body = match json {
        Value::Null => Vec::new(),
        Value::String(text) => text.into_bytes(),
        json => serde_json::to_vec(&json).unwrap(),
    };
    BytesResponse::from(response.body(Full::new(Bytes::from(body))).unwrap())
}

/// A request whose answer the network lost.
fn lost() -> instant_acme::Error {
    instant_acme::Error::Other("connection reset".into())
}

/// The DER certificate request inside a finalize request's JWS payload.
fn finalize_request(body: &[u8]) -> Vec<u8> {
    let jws: Value = serde_json::from_slice(body).unwrap();
    let payload: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(jws["payload"].as_str().unwrap()).unwrap()).unwrap();
    URL_SAFE_NO_PAD.decode(payload["csr"].as_str().unwrap()).unwrap()
}

/// A PEM chain from a fresh CA, for the key `request` carries, naming `names`, valid for 90 days.
fn issue(names: &str, request: &[u8]) -> String {
    let (_, request) = X509CertificationRequest::from_der(request).unwrap();
    let key = RequestedKey(request.certification_request_info.subject_pki.subject_public_key.data.to_vec());
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.distinguished_name.push(DnType::CommonName, "fake CA");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let root = ca.self_signed(&ca_key).unwrap();
    let mut leaf = CertificateParams::new(vec![names.to_owned()]).unwrap();
    leaf.not_before = date_time_ymd(2026, 10, 1);
    leaf.not_after = date_time_ymd(2026, 12, 30);
    let leaf = leaf.signed_by(&key, &Issuer::new(ca, &ca_key)).unwrap();
    format!("{}{}", leaf.pem(), root.pem())
}
