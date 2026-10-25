//! `engine admin`: signs an admin command with the operator's key and submits it to a node's
//! `POST /v1/admin`, over TLS pinned to the node's attested key. The key comes from a file or the
//! `OPERATOR_KEY` environment variable, never the command line, where other users of the machine
//! could read it, and it never leaves this process: the node holds only the operator's public key.

mod benchmark_feed;
mod benchmark_watch;
mod tls;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use clap::{ArgAction, Args, Subcommand};
use engine_core::auth::{MAX_EXPIRY_AHEAD_MS, Operator, RequestAuth};
use engine_core::command::{AdminCommand, AdminRequest};
use engine_core::corporate::CorporateAction;
use engine_core::eip712;
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Millis};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{CONTENT_TYPE, HOST};
use hyper::http::uri::Authority;
use hyper::{Method, Request, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use k256::ecdsa::SigningKey;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::time::timeout;

use crate::clock::now_ms;
use crate::config::NodeConfig;

/// How long one call to the node may take, connection included.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// How far ahead a signed command expires: half the engine's bound, so it is accepted with this
/// machine's clock up to two and a half minutes off the node's either way.
const EXPIRES_IN_MS: Millis = MAX_EXPIRY_AHEAD_MS / 2;

#[derive(Args)]
pub struct AdminArgs {
    /// The node's config, which names the operator's public key and the custody chain and vault
    /// that admin commands are signed for.
    #[arg(short, long, env = "CONFIG_PATH", default_value = "config/dev.toml")]
    config: PathBuf,
    /// The node's API, as `https://host:port`.
    #[arg(short, long, default_value = "https://127.0.0.1:8080")]
    node: Uri,
    /// The node's attested TLS key hash, in hex: the last 32 bytes of the `bundle` its
    /// `/v1/attestation` serves, once the bundle is checked against the quote's report data. The
    /// connection is refused for any other key.
    #[arg(short, long, value_parser = tls::parse_pin)]
    pin: [u8; 32],
    /// A file holding the operator's private key in hex. Without it, `OPERATOR_KEY_FILE` names the
    /// file, or `OPERATOR_KEY` holds the key.
    #[arg(short, long)]
    key_file: Option<PathBuf>,
    #[command(subcommand)]
    action: AdminAction,
}

/// The admin commands the operator may send.
#[derive(Subcommand)]
pub enum AdminAction {
    /// Schedule a corporate action from TOML, before its previous-close cutoff.
    ScheduleCorporateAction { file: PathBuf },
    /// List a market, or replace a listed market's spec and calendar, from a TOML market spec.
    UpsertMarket { file: PathBuf },
    /// Replace the protocol parameters from a TOML file.
    SetParams { file: PathBuf },
    /// Set a benchmark rate, in bps a year.
    SetBenchmarkRate {
        benchmark: String,
        #[arg(allow_negative_numbers = true)]
        annual_bps: i64,
    },
    /// Fetch publisher histories and sequence their effective dates through the signed operator channel.
    SyncBenchmarks {
        /// Repeat at benchmarks.interval; run as a separate operator service.
        #[arg(long)]
        watch: bool,
    },
    /// Set the operator's stress floor for one market, or for every market without `--market`.
    /// A market's level never falls below what its automatic triggers set.
    SetStressFloor {
        level: usize,
        #[arg(short, long)]
        market: Option<MarketId>,
    },
    /// Stop (`true`) or allow (`false`) new risk in one market, or in every market without
    /// `--market`. Trades that reduce or close a position are always allowed.
    SetReduceOnly {
        #[arg(action = ArgAction::Set)]
        reduce_only: bool,
        #[arg(short, long)]
        market: Option<MarketId>,
    },
    /// Record the MONKEY emission schedule, which a log takes once: period 0 starts at `--start`,
    /// no earlier than the node sequences the command, with `--period-budget` base units.
    InitEmissions {
        /// When period 0 starts, in RFC 3339 (`2027-01-01T00:00:00Z`).
        #[arg(long)]
        start: DateTime<Utc>,
        /// Period 0's budget in MONKEY base units (18 decimals); budgets halve every 182.5 days.
        #[arg(long)]
        period_budget: TokenUnits,
    },
    /// Record the MONKEY staking config, which a log takes once: epoch 0 starts at `--start`, no
    /// earlier than the node sequences the command, with the house NAV `--liquidity-floor`, epochs
    /// of `--epoch-ms` milliseconds, and the unstake `--cooldown-ms` the staking contract enforces.
    InitStaking {
        /// When epoch 0 starts, in RFC 3339 (`2027-01-01T00:00:00Z`).
        #[arg(long)]
        start: DateTime<Utc>,
        /// House NAV at or below this distributes nothing, in whole units.
        #[arg(long)]
        liquidity_floor: engine_core::Fixed,
        /// One epoch's length in milliseconds.
        #[arg(long)]
        epoch_ms: Millis,
        /// The unstake cooldown the staking contract enforces, in milliseconds.
        #[arg(long)]
        cooldown_ms: Millis,
    },
}

/// A node's API: its `https://host:port` URL, and the hash its TLS key must have.
pub(crate) struct NodeApi {
    pub url: Uri,
    pub pin: [u8; 32],
}

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
struct NonceView {
    next_nonce: u64,
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

impl AdminAction {
    /// The admin command this action sends.
    /// Error cases:
    /// - a spec or parameters file cannot be read, does not parse, or fails validation
    fn command(self) -> anyhow::Result<AdminCommand> {
        Ok(match self {
            AdminAction::ScheduleCorporateAction { file } => {
                let action: CorporateAction = read_toml(&file)?;
                action.validate().map_err(anyhow::Error::msg)?;
                AdminCommand::ScheduleCorporateAction(action)
            }
            AdminAction::UpsertMarket { file } => {
                let spec: MarketSpec = read_toml(&file)?;
                spec.validate().map_err(anyhow::Error::msg).with_context(|| file.display().to_string())?;
                AdminCommand::UpsertMarket(spec)
            }
            AdminAction::SetParams { file } => {
                let params: ProtocolParams = read_toml(&file)?;
                // The node runs the latest version, so these are the parameters it would apply.
                params
                    .validate_for(ProtocolVersion::LATEST)
                    .and_then(|()| params.check_closing_spread())
                    .and_then(|()| params.check_notional_weighting())
                    .map_err(anyhow::Error::msg)
                    .with_context(|| file.display().to_string())?;
                AdminCommand::SetParams(params)
            }
            AdminAction::SetBenchmarkRate { benchmark, annual_bps } => {
                AdminCommand::SetBenchmarkRate { benchmark, annual_bps }
            }
            AdminAction::SyncBenchmarks { .. } => bail!("publisher sync is asynchronous"),
            AdminAction::SetStressFloor { level, market } => AdminCommand::SetStressFloor { market, level },
            AdminAction::SetReduceOnly { reduce_only, market } => AdminCommand::SetReduceOnly { market, reduce_only },
            AdminAction::InitEmissions { start, period_budget } => {
                AdminCommand::InitEmissions(EmissionSchedule { start_ms: start.timestamp_millis(), period_budget })
            }
            AdminAction::InitStaking { start, liquidity_floor, epoch_ms, cooldown_ms } => {
                AdminCommand::InitStaking(engine_core::staking::StakingConfig {
                    liquidity_floor,
                    epoch_ms,
                    start_ms: start.timestamp_millis(),
                    cooldown_ms,
                })
            }
        })
    }
}

/// Signs the command with the operator's next nonce and submits it, printing the signed request and
/// the events the engine produced. Returns the request it sent.
/// Error cases:
/// - the config does not load, or the key is missing, unreadable, malformed, or not the config's
///   operator key
/// - a spec or parameters file cannot be read, does not parse, or fails validation
/// - the node cannot be reached in time, or refuses the request (see `call`)
/// - the engine rejects the command
pub async fn run(args: AdminArgs, env: KeyEnv) -> anyhow::Result<AdminRequest> {
    let cfg = NodeConfig::load(&args.config)?;
    crate::init_logging(&cfg.logging.level)?;
    run_configured(args, env, now_ms, cfg).await
}

/// `run`, with expiries counted from `clock`.
/// Error cases:
/// - see `run`
#[cfg(test)]
pub(crate) async fn run_with_clock(
    args: AdminArgs,
    env: KeyEnv,
    clock: impl Fn() -> Millis,
) -> anyhow::Result<AdminRequest> {
    let cfg = NodeConfig::load(&args.config)?;
    run_configured(args, env, clock, cfg).await
}

/// Runs one configured operator client with its explicit clock.
/// Error cases:
/// - see `run`
async fn run_configured(
    args: AdminArgs,
    env: KeyEnv,
    clock: impl Fn() -> Millis,
    cfg: NodeConfig,
) -> anyhow::Result<AdminRequest> {
    let operator = cfg.operator()?;
    let key = resolve_key(args.key_file.as_deref(), env)?;
    if eip712::address(key.verifying_key()) != operator.address {
        bail!("the key is not the operator key {} in {}", operator.address, args.config.display());
    }
    let node = NodeApi { url: args.node, pin: args.pin };
    if let AdminAction::SyncBenchmarks { watch } = args.action {
        return benchmark_feed::run(&cfg, &node, &key, &operator, clock, watch).await;
    }
    let command = args.action.command()?;
    let path = format!("/v1/accounts/{}/nonce", operator.address);
    let NonceView { next_nonce } = call(&node, Method::GET, &path, None).await?;
    let req = sign(&key, &operator, command, next_nonce, clock() + EXPIRES_IN_MS);
    println!("{}", serde_json::to_string(&req)?);
    let events = submit(&node, &req).await?;
    println!("{}", serde_json::to_string(&events)?);
    if let Some(Event::AdminRejected { reason }) = events.last() {
        bail!("the engine rejected the command: {reason}");
    }
    Ok(req)
}

/// `command` signed by `key` for `operator`'s domain, with `nonce`, to expire at `expires_at`.
pub(crate) fn sign(
    key: &SigningKey,
    operator: &Operator,
    command: AdminCommand,
    nonce: u64,
    expires_at: Millis,
) -> AdminRequest {
    let domain = eip712::domain(operator.chain_id, operator.vault);
    let hash = eip712::admin_hash(&domain, &command, nonce, expires_at);
    AdminRequest { command, auth: RequestAuth { nonce, expires_at, signature: eip712::sign(key, &hash) } }
}

/// Posts a signed admin request to the node and returns the events it produced.
/// Error cases:
/// - see `call`
pub(crate) async fn submit(node: &NodeApi, req: &AdminRequest) -> anyhow::Result<Vec<Event>> {
    call(node, Method::POST, "/v1/admin", Some(serde_json::to_string(req)?)).await
}

/// The operator's signing key: from the `--key-file` flag if given, else `env`'s file, else `env`'s key.
/// Error cases:
/// - see `operator_key`
fn resolve_key(flag: Option<&Path>, env: KeyEnv) -> anyhow::Result<SigningKey> {
    operator_key(flag.or(env.key_file.as_deref()), env.key)
}

/// The operator's signing key: from `key_file` if given, else `env_key`, the value of
/// `OPERATOR_KEY`.
/// Error cases:
/// - neither is given, or the file cannot be read
/// - the key is not a secp256k1 private key in hex
fn operator_key(key_file: Option<&Path>, env_key: Option<HexKey>) -> anyhow::Result<SigningKey> {
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
fn read_toml<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The host and port to connect to for `authority`: an IPv6 literal without its URL brackets, which
/// the `Host` header keeps, and port 443 when the URL names none.
fn socket_target(authority: &Authority) -> (&str, u16) {
    let host = authority.host();
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    (host, authority.port_u16().unwrap_or(443))
}

/// Makes one HTTP/1.1 call to the node over TLS pinned to its key, and parses its JSON reply.
/// Error cases:
/// - the node URL is not an `https://host:port` URL: another scheme, or a path or query
/// - the node cannot be reached, its TLS key is not the pinned one, or it does not answer within
///   `CALL_TIMEOUT`
/// - a status other than 200, reported with the node's error body
/// - a reply that is not the expected JSON
async fn call<T: DeserializeOwned>(
    node: &NodeApi,
    method: Method,
    path: &str,
    body: Option<String>,
) -> anyhow::Result<T> {
    let url = &node.url;
    let (Some("https"), Some(authority), "/", None) = (url.scheme_str(), url.authority(), url.path(), url.query())
    else {
        bail!("the node URL must be https://host:port, not {url}");
    };
    let target = format!("{method} {url}{}", path.trim_start_matches('/'));
    let exchange = async {
        let (host, port) = socket_target(authority);
        let stream = tls::connect(host, port, node.pin).await?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, authority.as_str())
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body.unwrap_or_default())))?;
        // The connection closes once the response is read and `sender` is dropped.
        let response = async move {
            let response = sender.send_request(request).await?;
            let status = response.status();
            anyhow::Ok((status, response.into_body().collect().await?.to_bytes()))
        };
        let (response, _) = tokio::join!(response, connection);
        response
    };
    let (status, reply) = timeout(CALL_TIMEOUT, exchange)
        .await
        .with_context(|| format!("{target}: no answer within {CALL_TIMEOUT:?}"))?
        .with_context(|| target.clone())?;
    if status != StatusCode::OK {
        bail!("{target}: {status} {}", String::from_utf8_lossy(&reply));
    }
    serde_json::from_slice(&reply).with_context(|| format!("{target}: unexpected reply"))
}

#[cfg(test)]
mod tests;
