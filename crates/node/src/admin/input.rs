//! Local input for `engine admin`: the operator key from a file or the environment (never the
//! command line), TOML command payloads, the nonce reply, and the connection target a node URL
//! names.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use hyper::http::uri::Authority;
use k256::ecdsa::SigningKey;
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// Where the operator's key may come from besides `--key-file`: the environment, which `main` reads
/// and passes in, so tests give their own.
pub struct KeyEnv {
    /// `OPERATOR_KEY_FILE`: a file holding the key.
    pub key_file: Option<PathBuf>,
    /// `OPERATOR_KEY`: the key itself.
    pub key: Option<HexKey>,
}

/// A private key in hex, as read from a file or the environment. `Debug` and `Display` hide it.
pub struct HexKey(pub String);

/// The body of `GET /v1/accounts/{address}/nonce`.
#[derive(Deserialize)]
pub(super) struct NonceView {
    pub(super) next_nonce: u64,
}

impl fmt::Debug for HexKey {
    /// Redacted.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HexKey(<redacted>)")
    }
}

impl fmt::Display for HexKey {
    /// Redacted.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The operator's signing key: from the `--key-file` flag if given, else `env`'s file, else `env`'s key.
/// Error cases:
/// - see `operator_key`
pub(super) fn resolve_key(flag: Option<&Path>, env: KeyEnv) -> anyhow::Result<SigningKey> {
    operator_key(flag.or(env.key_file.as_deref()), env.key)
}

/// The operator's signing key: from `key_file` if given, else `env_key`, the value of
/// `OPERATOR_KEY`.
/// Error cases:
/// - neither is given, or the file cannot be read
/// - the key is not a secp256k1 private key in hex
pub(super) fn operator_key(key_file: Option<&Path>, env_key: Option<HexKey>) -> anyhow::Result<SigningKey> {
    let HexKey(text) = match key_file {
        Some(path) => HexKey(std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?),
        None => env_key.context("no operator key: pass --key-file, or set OPERATOR_KEY_FILE or OPERATOR_KEY")?,
    };
    // The errors name neither the key nor the bytes that failed to parse.
    hex::decode(text.trim().trim_start_matches("0x"))
        .ok()
        .and_then(|bytes| SigningKey::from_slice(&bytes).ok())
        .context("the operator key is not a secp256k1 private key in hex")
}

/// Reads a TOML file into `T`.
/// Error cases:
/// - the file cannot be read, or is not valid TOML for `T` (unknown keys included)
pub(super) fn read_toml<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The host and port to connect to for `authority`: an IPv6 literal without its URL brackets, which
/// the `Host` header keeps, and port 443 when the URL names none.
pub(super) fn socket_target(authority: &Authority) -> (&str, u16) {
    let host = authority.host();
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    (host, authority.port_u16().unwrap_or(443))
}
