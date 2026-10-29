//! The engine's verifier for browsers, built for WebAssembly with wasm-bindgen. A page verifies the
//! node's attestation, then every signed response and stream frame it relies on, with the code
//! `engine verify-attestation`, `engine verify-response`, and `engine verify-frames` run:
//! `engine_verify` and `engine_wire`. Inputs are the bodies and header values exactly as received;
//! a failed check throws its message as a string.

use anyhow::Context as _;
use engine_verify::{Attestation, RelayedCollateral, ReleaseManifest};
use engine_wire::{Connection, Deployment, Received, SentRequest};
use wasm_bindgen::prelude::*;

/// What an attestation proves: the enclave whose signed answers the page then accepts.
#[wasm_bindgen]
pub struct VerifiedAttestation {
    /// True only for a mock attestation read by `verifyMockAttestation`, where nothing proves an
    /// enclave; a page that accepts one says so on screen.
    #[wasm_bindgen(readonly)]
    pub mock: bool,
    /// The release manifest that admitted the quote; absent for a mock.
    #[wasm_bindgen(readonly, getter_with_clone, js_name = releaseName)]
    pub release_name: Option<String>,
    /// The release's name for the node's configuration file; absent for a mock.
    #[wasm_bindgen(readonly, getter_with_clone, js_name = configName)]
    pub config_name: Option<String>,
    /// `0x` and 40 lower-case hex digits: the address that signs the enclave's answers.
    #[wasm_bindgen(readonly, getter_with_clone, js_name = enclaveAddress)]
    pub enclave_address: String,
    /// `0x` and 64 hex digits: SHA-256 of the API TLS key's DER SubjectPublicKeyInfo.
    #[wasm_bindgen(readonly, getter_with_clone, js_name = tlsPublicKeySha256)]
    pub tls_public_key_sha256: String,
    /// The first Unix millisecond at which every date check passes; absent for a mock. Dates are
    /// checked in whole seconds, so `nowMs` verifies exactly from `validFromMs` to `validUntilMs`.
    #[wasm_bindgen(readonly, js_name = validFromMs)]
    pub valid_from_ms: Option<f64>,
    /// The last Unix millisecond at which every date check passes; absent for a mock. A clock
    /// outside the window, such as the node's signed time, would not have given this verdict.
    #[wasm_bindgen(readonly, js_name = validUntilMs)]
    pub valid_until_ms: Option<f64>,
}

/// Who must have signed an answer, for which deployment, and the challenge the request carried.
#[wasm_bindgen]
pub struct ExpectedSigner {
    signer: [u8; 20],
    deployment: Deployment,
    challenge: [u8; 32],
}

/// What a verified response's signature vouches for besides its body.
#[wasm_bindgen]
pub struct VerifiedResponse {
    #[wasm_bindgen(readonly)]
    pub status: u16,
    /// The node's time when it answered, in Unix milliseconds.
    #[wasm_bindgen(readonly, js_name = timeMs)]
    pub time_ms: f64,
    /// The log head's seq when it answered.
    #[wasm_bindgen(readonly, js_name = headSeq)]
    pub head_seq: u64,
}

/// Checks one signed stream connection's frames in the order received. Once a frame fails, every
/// later one does too, and the page closes the connection.
#[wasm_bindgen]
pub struct FrameVerifier {
    inner: engine_wire::FrameVerifier,
}

#[wasm_bindgen]
impl ExpectedSigner {
    /// `signer` is the verified attestation's `enclaveAddress`, `chainId` and `vault` the
    /// deployment's, and `challenge` the 64 hex digits the request or connection sent.
    /// Error cases:
    /// - an address is not `0x` and 40 hex digits, or the challenge is not 64 hex digits
    #[wasm_bindgen(constructor)]
    pub fn new(signer: &str, chain_id: u64, vault: &str, challenge: &str) -> Result<ExpectedSigner, String> {
        Ok(Self {
            signer: address(signer, "signer")?,
            deployment: Deployment { chain_id, vault: address(vault, "vault")? },
            challenge: engine_wire::parse_challenge(challenge)
                .ok_or_else(|| "the challenge is not 64 hex digits, without 0x".to_owned())?,
        })
    }
}

#[wasm_bindgen]
impl FrameVerifier {
    /// A verifier for the connection that `GET` of `pathAndQuery`, exactly as sent, opened with
    /// `expected`'s challenge.
    #[wasm_bindgen(constructor)]
    pub fn new(expected: &ExpectedSigner, path_and_query: &str) -> FrameVerifier {
        let connection = Connection::new(expected.challenge, path_and_query);
        Self { inner: engine_wire::FrameVerifier::new(expected.signer, expected.deployment, connection) }
    }

    /// Checks the connection's next frame, its JSON text as received, and returns its message: the
    /// unsigned stream's frame, as JSON text.
    /// Error cases:
    /// - see `engine_wire::FrameVerifier::verify_text`
    #[wasm_bindgen(js_name = verifyFrame)]
    pub fn verify_frame(&mut self, frame: &str) -> Result<String, String> {
        self.inner.verify_text(frame).map(|frame| frame.msg).map_err(|err| err.to_string())
    }
}

/// Verifies the attestation `/v1/attestation` served against the release manifests' TOML texts, with
/// the collateral `/v1/attestation/collateral` served, checking every date at `nowMs`, and pinning
/// the chain to the vendor roots plus `extraRootPem` when given (a test or private chain only). A
/// mock quote never verifies.
/// Error cases:
/// - `nowMs` is not a finite number, or an input does not parse
/// - see `engine_verify::verify_attestation`
#[wasm_bindgen(js_name = verifyAttestation)]
pub fn verify_attestation(
    attestation: &str,
    manifests: Vec<String>,
    collateral: &str,
    now_ms: f64,
    extra_root_pem: Option<String>,
) -> Result<VerifiedAttestation, String> {
    let verify = || -> anyhow::Result<VerifiedAttestation> {
        anyhow::ensure!(now_ms.is_finite(), "nowMs is not a finite number");
        let releases = manifests
            .iter()
            .enumerate()
            .map(|(index, text)| ReleaseManifest::from_toml(text).with_context(|| format!("parsing manifest {index}")))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let collateral = RelayedCollateral::from_json(collateral.as_bytes())?;
        // Certificate and list dates are whole seconds: the window below covers every millisecond
        // of its first and last second.
        let now = (now_ms / 1000.0).floor() as i64;
        let verified = engine_verify::verify_attestation(
            &parse_attestation(attestation)?,
            &releases,
            &collateral,
            extra_root_pem.as_deref(),
            now,
        )?;
        Ok(VerifiedAttestation {
            mock: false,
            release_name: Some(verified.release_name),
            config_name: Some(verified.config_name),
            enclave_address: hex0x(&verified.enclave_address),
            tls_public_key_sha256: hex0x(&verified.tls_public_key_sha256),
            valid_from_ms: Some(verified.validity.from as f64 * 1000.0),
            valid_until_ms: Some(verified.validity.until as f64 * 1000.0 + 999.0),
        })
    };
    verify().map_err(|err| format!("{err:#}"))
}

/// Reads a mock attestation, for a development node only: the result says `mock`, and no release,
/// configuration, or time window vouches for it. A page calls this only in a development build and
/// shows that the attestation is a mock.
/// Error cases:
/// - the attestation does not parse
/// - see `engine_verify::verify_mock_attestation`
#[wasm_bindgen(js_name = verifyMockAttestation)]
pub fn verify_mock_attestation(attestation: &str) -> Result<VerifiedAttestation, String> {
    let read = || -> anyhow::Result<VerifiedAttestation> {
        let bundle = engine_verify::verify_mock_attestation(&parse_attestation(attestation)?)?;
        Ok(VerifiedAttestation {
            mock: true,
            release_name: None,
            config_name: None,
            enclave_address: hex0x(&bundle.enclave_address),
            tls_public_key_sha256: hex0x(&bundle.tls_public_key_sha256),
            valid_from_ms: None,
            valid_until_ms: None,
        })
    };
    read().map_err(|err| format!("{err:#}"))
}

/// Verifies a response as `expected`'s answer to the request sent: its method, path and query, and
/// body exactly as sent; the status; the values of `X-Engine-Signature`, `X-Engine-Time`,
/// `X-Engine-Seq`, and `X-Engine-Challenge` (`null` or `undefined` when absent); and the body as
/// received. A response without a signature is never relied on.
/// Error cases:
/// - see `engine_wire::verify_received`
#[wasm_bindgen(js_name = verifyResponse)]
#[allow(clippy::too_many_arguments)]
pub fn verify_response(
    expected: &ExpectedSigner,
    method: &str,
    path_and_query: &str,
    request_body: &[u8],
    status: u16,
    signature: Option<String>,
    time: Option<String>,
    seq: Option<String>,
    challenge: Option<String>,
    body: &[u8],
) -> Result<VerifiedResponse, String> {
    let request = SentRequest { method, path_and_query, body: request_body };
    let received = Received {
        status,
        signature: signature.as_deref(),
        time: time.as_deref(),
        seq: seq.as_deref(),
        challenge: challenge.as_deref(),
        body,
    };
    let verified =
        engine_wire::verify_received(&expected.signer, &expected.deployment, &expected.challenge, &request, &received)
            .map_err(|err| err.to_string())?;
    Ok(VerifiedResponse { status: verified.status, time_ms: verified.time_ms as f64, head_seq: verified.head_seq })
}

/// Parses the attestation JSON.
/// Error cases:
/// - the text is not the attestation's JSON shape
fn parse_attestation(text: &str) -> anyhow::Result<Attestation> {
    serde_json::from_str(text).context("parsing the attestation")
}

/// Parses a `0x`-prefixed address of 40 hex digits in either case.
/// Error cases:
/// - no `0x` prefix, or not 40 hex digits after it
fn address(text: &str, what: &str) -> Result<[u8; 20], String> {
    let mut bytes = [0; 20];
    text.strip_prefix("0x")
        .and_then(|hex| hex::decode_to_slice(hex, &mut bytes).ok())
        .ok_or_else(|| format!("the {what} is not 0x and 40 hex digits"))?;
    Ok(bytes)
}

/// `0x` and the lower-case hex of `bytes`.
fn hex0x(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

#[cfg(test)]
mod tests;
