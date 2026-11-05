//! The beacon node's API: the finalized block's header and its state. Both come from a node the host
//! reaches, so neither is trusted. The header counts only once its root matches the EIP-4788 entry
//! the Ethereum quorum reads, the state only once it hashes to the header's `state_root`, and
//! `SeedRelay` checks both again on chain. Each read is bounded in time and size, TLS trusts only the
//! roots compiled into this binary, and errors never carry the URL, which may hold a key.

use std::time::Duration;

use anyhow::{Context, anyhow, ensure};
use reqwest::Url;
use reqwest::header::ACCEPT;
use reqwest::{Certificate, Client, Response, redirect};
use serde::Deserialize;
use tokio::time::timeout;

use crate::config::{SeedRelaySection, redact};
use crate::seed_relay::BeaconBlockHeader;

/// The most a header answer may hold.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// The most a state may hold: a mainnet Fulu state is about 340 MB.
pub(crate) const MAX_STATE_BYTES: usize = 1 << 30;

/// Reads headers and states from one beacon node.
pub(crate) struct BeaconApi {
    http: Client,
    base: Url,
    redactions: Vec<String>,
    timeout: Duration,
    state_timeout: Duration,
}

#[derive(Deserialize)]
struct HeaderAnswer {
    data: HeaderData,
}

#[derive(Deserialize)]
struct HeaderData {
    header: SignedHeader,
}

#[derive(Deserialize)]
struct SignedHeader {
    message: BeaconBlockHeader,
}

impl BeaconApi {
    /// Builds the client without a request.
    /// Error cases:
    /// - the URL is not delivered or does not parse, or the TLS roots or client do not build
    pub(crate) fn new(cfg: &SeedRelaySection) -> anyhow::Result<Self> {
        let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|cert| Certificate::from_der(cert))
            .collect::<Result<Vec<_>, _>>()
            .context("loading the beacon API's TLS roots")?;
        let http = Client::builder()
            .user_agent(concat!("engine-seed-relay/", env!("CARGO_PKG_VERSION")))
            .tls_certs_only(roots)
            .no_proxy()
            // A redirect would leave the origin the attested config fixes, carrying any key with it.
            .redirect(redirect::Policy::none())
            .connect_timeout(cfg.rpc_timeout)
            .build()
            .context("building the beacon API client")?;
        Ok(Self {
            http,
            base: cfg.beacon_api.expose()?,
            redactions: cfg.beacon_api.redactions()?,
            timeout: cfg.rpc_timeout,
            state_timeout: cfg.state_timeout,
        })
    }

    /// The finalized block's header.
    /// Error cases:
    /// - the request fails, times out, answers other than 200, or answers more than 64 KiB or not a
    ///   header
    pub(crate) async fn finalized_header(&self) -> anyhow::Result<BeaconBlockHeader> {
        let url = self.url("eth/v1/beacon/headers/finalized");
        let body =
            self.read(self.http.get(url), self.timeout, MAX_HEADER_BYTES).await.context("the finalized header")?;
        let answer: HeaderAnswer = serde_json::from_slice(&body).context("decoding the finalized header")?;
        Ok(answer.data.header.message)
    }

    /// The state after the block at `slot`, serialized; the beacon node must name it a Fulu state.
    /// Error cases:
    /// - the request fails, takes longer than `state_timeout`, answers other than 200, names another
    ///   fork, or answers more than `MAX_STATE_BYTES`
    pub(crate) async fn state(&self, slot: u64) -> anyhow::Result<Vec<u8>> {
        let url = self.url(&format!("eth/v2/debug/beacon/states/{slot}"));
        let request = self.http.get(url).header(ACCEPT, "application/octet-stream");
        let read = async {
            let response = self.send(request).await?;
            let version = response.headers().get("eth-consensus-version").and_then(|v| v.to_str().ok());
            // A fork's state needs a node release that reads it, and a relay that accepts its version.
            ensure!(
                version == Some("fulu"),
                "the beacon node serves a {version:?} state; this node reads Fulu states only"
            );
            self.body(response, MAX_STATE_BYTES).await
        };
        timeout(self.state_timeout, read)
            .await
            .map_err(|_| anyhow!("the state at slot {slot} took longer than {:?}", self.state_timeout))?
            .with_context(|| format!("the state at slot {slot}"))
    }

    /// `path` under the configured base, keeping the base's query, where a delivered key may sit.
    fn url(&self, path: &str) -> Url {
        let mut url = self.base.clone();
        let joined = format!("{}/{path}", url.path().trim_end_matches('/'));
        url.set_path(&joined);
        url
    }

    /// The body of a request that must answer within `deadline` and with at most `limit` bytes.
    /// Error cases:
    /// - see `send` and `body`, or the deadline passes
    async fn read(
        &self,
        request: reqwest::RequestBuilder,
        deadline: Duration,
        limit: usize,
    ) -> anyhow::Result<Vec<u8>> {
        timeout(deadline, async { self.body(self.send(request).await?, limit).await })
            .await
            .map_err(|_| anyhow!("the beacon API took longer than {deadline:?}"))?
    }

    /// Sends a request, refusing an answer other than 200.
    /// Error cases:
    /// - the transport fails or the status is not 200
    async fn send(&self, request: reqwest::RequestBuilder) -> anyhow::Result<Response> {
        let response = request
            .send()
            .await
            .map_err(|e| anyhow!("the beacon API failed: {}", redact(&e.to_string(), &self.redactions)))?;
        ensure!(response.status() == reqwest::StatusCode::OK, "the beacon API answered {}", response.status());
        Ok(response)
    }

    /// Reads at most `limit` bytes of a body.
    /// Error cases:
    /// - the transport fails mid-body, or the body is longer than `limit`
    async fn body(&self, mut response: Response, limit: usize) -> anyhow::Result<Vec<u8>> {
        // The length the node declares, never past `limit`, saves regrowing a state's buffer.
        let declared = response.content_length().map_or(0, |length| usize::try_from(length).unwrap_or(limit));
        let mut bytes = Vec::with_capacity(declared.min(limit));
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| anyhow!("reading the beacon API: {}", redact(&e.to_string(), &self.redactions)))?
        {
            ensure!(bytes.len() + chunk.len() <= limit, "the beacon API answered more than {limit} bytes");
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key may sit in the base URL's path or its query; every route keeps both.
    #[test]
    fn routes_keep_the_base_path_and_query() {
        let mut cfg: SeedRelaySection = toml::from_str(
            "chain_id = 1\nrpc_url = 'http://127.0.0.1:4'\n\
             relayer = '0x2222222222222222222222222222222222222222'\n\
             beacon_api = 'https://beacon.example/node/?api_key={beacon_key}'\n\
             relay = '0x4444444444444444444444444444444444444444'\n\
             receiver = '0x5555555555555555555555555555555555555555'\n\
             poll_interval = '60s'\nrpc_timeout = '10s'\nstate_timeout = '300s'\nresend_after = '1h'\n\
             l2_max_fee_per_gas = '0.1'\nmin_balance = '0.05'\nproviders = []",
        )
        .unwrap();
        cfg.beacon_api.deliver_test_key("K");
        let api = BeaconApi::new(&cfg).unwrap();
        assert_eq!(
            api.url("eth/v1/beacon/headers/finalized").as_str(),
            "https://beacon.example/node/eth/v1/beacon/headers/finalized?api_key=K"
        );
        assert_eq!(
            api.url("eth/v2/debug/beacon/states/7").as_str(),
            "https://beacon.example/node/eth/v2/debug/beacon/states/7?api_key=K"
        );
    }
}
