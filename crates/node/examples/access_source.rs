//! Loopback-only, explicitly synthetic access-source fixture for the Anvil scenario.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, ensure};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use engine_core::access::{Jurisdiction, SanctionsStatus, Screening};
use engine_core::eip712::{self, B256, Eip712Domain, Signature};
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use serde::Deserialize;
use tokio::net::TcpListener;

/// Anvil account 0, already listed in config/test-keys.toml; never a production source authority.
const FIXTURE_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    request_hash: B256,
    account: Option<AccountId>,
    payer: Option<AccountId>,
    peer: Option<IpAddr>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    mode: String,
    jurisdictions: BTreeMap<AccountId, Jurisdiction>,
    sanctioned: BTreeSet<AccountId>,
    /// Client addresses the scenario places in a jurisdiction, as a proxy's header names them. A peer
    /// listed here is a client; any other peer that is not loopback is refused.
    #[serde(default)]
    peers: BTreeMap<IpAddr, Jurisdiction>,
}

struct Source {
    path: PathBuf,
    domain: Eip712Domain,
    key: SigningKey,
}

/// Serves signed synthetic dispositions only on loopback.
/// Error cases:
/// - invalid arguments, non-loopback listener, or listener/server failure
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [listen, chain_id, vault, path] = args.as_slice() else {
        anyhow::bail!("usage: access_source <loopback listen> <chain id> <vault> <fixture JSON path>");
    };
    let listen: SocketAddr = listen.parse()?;
    ensure!(listen.ip().is_loopback(), "synthetic access fixtures require loopback");
    let source = Source {
        path: path.into(),
        domain: eip712::domain(chain_id.parse()?, vault.parse()?),
        key: SigningKey::from_slice(&hex::decode(FIXTURE_KEY)?).context("fixture key")?,
    };
    let app = Router::new().route("/", post(screen)).with_state(Arc::new(source)).layer(DefaultBodyLimit::max(4096));
    axum::serve(TcpListener::bind(listen).await?, app).await.context("fixture source")
}

/// Signs the fixture's decision; the account table exercises routing and is not geolocation.
async fn screen(State(source): State<Arc<Source>>, Json(query): Json<Query>) -> Response {
    let Ok(bytes) = tokio::fs::read(&source.path).await else { return StatusCode::SERVICE_UNAVAILABLE.into_response() };
    let Ok(fixture) = serde_json::from_slice::<Fixture>(&bytes) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let listed = query.peer.and_then(|peer| fixture.peers.get(&peer).copied());
    if fixture.mode == "unavailable" || query.peer.is_some_and(|peer| !peer.is_loopback() && listed.is_none()) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("Unix time").as_millis() as i64;
    let status = |address: Option<AccountId>| {
        if fixture.mode == "unknown" || address.is_none() {
            SanctionsStatus::Unknown
        } else if address.is_some_and(|address| fixture.sanctioned.contains(&address)) {
            SanctionsStatus::Sanctioned
        } else {
            SanctionsStatus::Clear
        }
    };
    let mut screening = Screening {
        source: eip712::address(source.key.verifying_key()),
        request_hash: query.request_hash,
        observed_at: now - if fixture.mode == "stale" { 60_000 } else { 0 },
        account: query.account,
        payer: query.payer,
        jurisdiction: if fixture.mode == "unknown" || query.peer.is_none() {
            Jurisdiction::Unknown
        } else {
            listed
                .or_else(|| query.account.and_then(|account| fixture.jurisdictions.get(&account).copied()))
                .unwrap_or(Jurisdiction::Allowed)
        },
        account_status: status(query.account),
        payer_status: status(query.payer),
        signature: Signature::ZERO,
    };
    if fixture.mode != "forged" {
        screening.signature = eip712::sign(&source.key, &eip712::screening_hash(&source.domain, &screening));
    }
    Json(screening).into_response()
}
