//! Checking a node's signed answers offline, as `engine verify-response` and `engine verify-frames`
//! do: a response saved with `curl -D <headers> -o <body>`, against the request the client sent, or
//! a signed stream connection's frames saved one per line from its first, against the request that
//! opened it; both against the attested enclave address, the deployment, and the client's
//! challenge. The digests and checks are `engine_wire`'s, which a browser runs too.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use engine_core::types::AccountId;
use engine_wire::{
    CHALLENGE_HEADER, Connection, Deployment, FrameVerifier, Response, SEQ_HEADER, SIGNATURE_HEADER, SignedFrame,
    TIME_HEADER, verify_response,
};

/// Who must have signed an answer, for which deployment, and under the challenge the client sent.
pub struct Expected {
    /// The enclave address a verified attestation names.
    pub signer: AccountId,
    pub chain_id: u64,
    pub vault: AccountId,
    pub challenge: [u8; 32],
}

/// The request a saved response answers, as the client sent it.
pub struct SentRequest<'a> {
    pub method: &'a str,
    pub path_and_query: &'a str,
    /// The request body; empty when it had none.
    pub body: &'a [u8],
}

/// What a verified response's signature vouches for besides its body.
#[derive(Debug, PartialEq, Eq)]
pub struct VerifiedResponse {
    pub status: u16,
    pub time_ms: u64,
    pub head_seq: u64,
}

impl Expected {
    fn deployment(&self) -> Deployment {
        Deployment { chain_id: self.chain_id, vault: self.vault.0 }
    }
}

/// Parses a challenge argument: 64 hex characters, as the client sent it.
/// Error cases:
/// - not exactly 64 hex characters
pub fn parse_challenge_arg(text: &str) -> Result<[u8; 32], String> {
    engine_wire::parse_challenge(text).ok_or_else(|| "a challenge is 64 hex characters, without 0x".to_owned())
}

/// Verifies the response whose status line and headers are in `headers` and whose body is in
/// `body`, as the answer to `request`.
/// Error cases:
/// - either file cannot be read
/// - see `verify_saved_response`
pub fn verify_response_files(
    expected: &Expected,
    request: &SentRequest<'_>,
    headers: &Path,
    body: &Path,
) -> anyhow::Result<VerifiedResponse> {
    let headers = std::fs::read_to_string(headers).with_context(|| format!("reading {}", headers.display()))?;
    let body = std::fs::read(body).with_context(|| format!("reading {}", body.display()))?;
    verify_saved_response(expected, request, &headers, &body)
}

/// Verifies the frames in `frames`, one JSON frame a line from the first of the connection that
/// `GET` of `path_and_query` opened, and returns how many there are.
/// Error cases:
/// - the file cannot be read
/// - see `verify_saved_frames`
pub fn verify_frames_file(expected: &Expected, path_and_query: &str, frames: &Path) -> anyhow::Result<u64> {
    let text = std::fs::read_to_string(frames).with_context(|| format!("reading {}", frames.display()))?;
    verify_saved_frames(expected, path_and_query, &text)
}

/// Verifies a response from its headers as `curl -D` writes them and its body as sent.
/// Error cases:
/// - the headers have no status line, or name a signature header twice
/// - the response carries no signature, time, or seq: an unsigned response is never relied on
/// - it echoes a challenge other than the one sent
/// - the signature is malformed, or not the signer's over this deployment, challenge, request, and
///   response
fn verify_saved_response(
    expected: &Expected,
    request: &SentRequest<'_>,
    headers: &str,
    body: &[u8],
) -> anyhow::Result<VerifiedResponse> {
    let (status, signed) = parse_headers(headers)?;
    let header = |name: &str| signed.get(name).with_context(|| format!("no {name}: the response is unsigned"));
    if let Some(echoed) = signed.get(CHALLENGE_HEADER) {
        ensure!(
            engine_wire::parse_challenge(echoed) == Some(expected.challenge),
            "the response echoes another challenge than the one sent"
        );
    }
    let time_ms = header(TIME_HEADER)?.parse().context(TIME_HEADER)?;
    let head_seq = header(SEQ_HEADER)?.parse().context(SEQ_HEADER)?;
    let response = Response {
        challenge: expected.challenge,
        time_ms,
        head_seq,
        status,
        method: request.method,
        path_and_query: request.path_and_query,
        request_body: request.body,
        body,
    };
    verify_response(&expected.signer.0, &expected.deployment(), &response, header(SIGNATURE_HEADER)?)?;
    Ok(VerifiedResponse { status, time_ms, head_seq })
}

/// Verifies the frames, one a line, of the connection `GET` of `path_and_query` opened, and returns
/// how many verified.
/// Error cases:
/// - there are no frames
/// - a line is not a signed frame, or its frame fails (see `FrameVerifier::verify`)
fn verify_saved_frames(expected: &Expected, path_and_query: &str, text: &str) -> anyhow::Result<u64> {
    let connection = Connection::new(expected.challenge, path_and_query);
    let mut verifier = FrameVerifier::new(expected.signer.0, expected.deployment(), connection);
    let mut count = 0;
    for (index, line) in text.lines().enumerate().filter(|(_, line)| !line.trim().is_empty()) {
        let frame: SignedFrame =
            serde_json::from_str(line).with_context(|| format!("line {}: not a signed frame", index + 1))?;
        verifier.verify(&frame).with_context(|| format!("line {}", index + 1))?;
        count += 1;
    }
    ensure!(count > 0, "no frames");
    Ok(count)
}

/// The status and the `X-Engine-*` headers of the last response in `text`, which may hold an
/// informational response before it, as `curl -D` writes them.
/// Error cases:
/// - no status line
/// - an `X-Engine-*` header appears twice
fn parse_headers(text: &str) -> anyhow::Result<(u16, BTreeMap<String, String>)> {
    let mut parsed = None;
    for line in text.lines().map(str::trim) {
        if let Some(status_line) = line.strip_prefix("HTTP/") {
            let status = status_line.split_whitespace().nth(1).and_then(|status| status.parse().ok());
            parsed = Some((status.with_context(|| format!("status line {line:?}"))?, BTreeMap::new()));
        } else if let (Some((_, headers)), Some((name, value))) = (parsed.as_mut(), line.split_once(':')) {
            let name = name.trim().to_ascii_lowercase();
            if [SIGNATURE_HEADER, TIME_HEADER, SEQ_HEADER, CHALLENGE_HEADER].contains(&name.as_str())
                && headers.insert(name.clone(), value.trim().to_owned()).is_some()
            {
                bail!("{name} appears twice");
            }
        }
    }
    parsed.context("no status line: not the headers curl -D writes")
}

#[cfg(test)]
mod tests;
