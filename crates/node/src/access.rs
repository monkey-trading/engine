//! Access-source IO. HTTPS trusts compiled roots; policy classifications are explicit provider inputs.
//!
//! TODO: bind source configuration into enclave attestation. A provider's classifications are trusted;
//! replay proves the decisions from logged inputs, not the correctness of the provider's database.

use std::net::IpAddr;

use anyhow::Context;
use engine_core::access::{AccessEvidence, Screening, SourceFailure};
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use reqwest::{Certificate, Client, Url, redirect};
use serde::Serialize;
use tokio::sync::Semaphore;
use tracing::warn;

use crate::config::AccessSection;

/// The source receives the real transport peer only for API actions; it is never logged.
#[derive(Serialize)]
pub(crate) struct ScreeningRequest {
    pub request_hash: B256,
    pub account: Option<AccountId>,
    pub payer: Option<AccountId>,
    pub peer: Option<IpAddr>,
}

pub(crate) struct AccessClient {
    http: Client,
    endpoint: Url,
    max_response_bytes: usize,
    pub(crate) api_slots: Semaphore,
}

impl AccessClient {
    /// Builds the shared, bounded client without contacting the configured source.
    /// Error cases:
    /// - invalid configuration, built-in TLS roots, or HTTP client construction
    pub(crate) fn new(config: &AccessSection) -> anyhow::Result<Self> {
        config.validate()?;
        let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|cert| Certificate::from_der(cert))
            .collect::<Result<Vec<_>, _>>()
            .context("loading access TLS roots")?;
        let http = Client::builder()
            .tls_certs_only(roots)
            .no_proxy()
            .redirect(redirect::Policy::none())
            .timeout(config.timeout)
            .connect_timeout(config.timeout)
            .build()
            .context("building access client")?;
        Ok(Self {
            http,
            endpoint: config.endpoint.expose()?,
            max_response_bytes: config.max_response_bytes,
            api_slots: Semaphore::new(config.max_in_flight.get()),
        })
    }

    /// Returns the actual response or a typed failure; transport errors cannot expose credentials.
    pub(crate) async fn screen(&self, request: &ScreeningRequest) -> AccessEvidence {
        match self.read(request).await {
            Ok(screening) => AccessEvidence::Screened(screening),
            Err(reason) => AccessEvidence::Failed { reason },
        }
    }

    /// Reads one response incrementally under the client's deadline and byte limit.
    /// Error cases:
    /// - timeout, transport/status failure, oversize body, malformed or mismatched response
    async fn read(&self, request: &ScreeningRequest) -> Result<Screening, SourceFailure> {
        // Logged without the URL, which may carry the source's credential.
        let unavailable = |error: reqwest::Error| {
            warn!(error = %error.without_url(), "access source request failed");
            SourceFailure::Unavailable
        };
        let mut response = self.http.post(self.endpoint.clone()).json(request).send().await.map_err(unavailable)?;
        if !response.status().is_success() {
            warn!(status = %response.status(), "access source answered with an error status");
            return Err(SourceFailure::Unavailable);
        }
        if response.content_length().is_some_and(|length| length > self.max_response_bytes as u64) {
            warn!("access source response exceeds max_response_bytes");
            return Err(SourceFailure::InvalidResponse);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(unavailable)? {
            if chunk.len() > self.max_response_bytes.saturating_sub(bytes.len()) {
                warn!("access source response exceeds max_response_bytes");
                return Err(SourceFailure::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk);
        }
        let screening: Screening = serde_json::from_slice(&bytes).map_err(|error| {
            warn!(%error, "access source response is not a screening");
            SourceFailure::InvalidResponse
        })?;
        if screening.request_hash != request.request_hash
            || screening.account != request.account
            || screening.payer != request.payer
        {
            warn!("access source answered about another request");
            return Err(SourceFailure::InvalidResponse);
        }
        Ok(screening)
    }
}

/// Missing configuration is a refusal, never an allow-all source.
pub(crate) async fn screen(client: Option<&AccessClient>, request: &ScreeningRequest) -> AccessEvidence {
    match client {
        Some(client) => client.screen(request).await,
        None => AccessEvidence::Failed { reason: SourceFailure::NotConfigured },
    }
}

#[cfg(test)]
mod tests;
