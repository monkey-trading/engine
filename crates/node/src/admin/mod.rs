//! `engine admin`: signs an admin command with the operator's key and submits it to a node's
//! `POST /v1/admin`, over TLS pinned to the node's attested key, or delivers secrets into its enclave
//! (`secrets`). The key comes from a file or the `OPERATOR_KEY` environment variable, never the
//! command line, where other users of the machine could read it, and it never leaves this process:
//! the node holds only the operator's public key.

mod benchmark_feed;
mod benchmark_watch;
mod input;
mod secrets;
mod tls;

pub use input::{HexKey, KeyEnv};

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
use engine_core::operator_bounds::{ParamsNotice, StressSettings};
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, MarketId, Millis};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{CONTENT_TYPE, HOST};
use hyper::{Method, Request, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use k256::ecdsa::SigningKey;
use serde::de::DeserializeOwned;
use tokio::time::timeout;

use crate::alerts::{Router, Routing};
use crate::clock::now_ms;
use crate::config::NodeConfig;
use crate::telemetry::{Telemetry, init_telemetry};

/// How long one call to the node may take, connection included.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// How far ahead a signed command expires: half the engine's bound, so it is accepted with this
/// machine's clock up to two and a half minutes off the node's either way.
const EXPIRES_IN_MS: Millis = MAX_EXPIRY_AHEAD_MS / 2;
/// How long the publisher worker waits on stopping for its alerts to reach their webhooks.
const ALERT_DRAIN_GRACE: Duration = Duration::from_secs(10);

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
    /// connection is refused for any other key. Required by every command but `deliver-secrets` and
    /// `secrets-status`, which check a quote instead and do not use the API.
    #[arg(short, long, value_parser = tls::parse_pin)]
    pin: Option<[u8; 32]>,
    /// A file holding the operator's private key in hex. Without it, `OPERATOR_KEY_FILE` names the
    /// file, or `OPERATOR_KEY` holds the key.
    #[arg(short, long)]
    key_file: Option<PathBuf>,
    /// The key is the one an operator rotation handed over to, not the config's `operator.public_key`.
    #[arg(long)]
    rotated: bool,
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
    /// Set a benchmark rate, in bps a year. From protocol V46 it moves at most 300 bps a day from the
    /// applied rates next to its date.
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
    /// A market's level never falls below what its automatic triggers set. From protocol V46 the
    /// floor is at most level 1 and lapses 72 hours after it is set, unless it is set again.
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
    /// Give notice of a new profit fee and/or close-out level (from V35), and of new stress
    /// parameters (from V49): a `set-params` carrying them applies only from `--effective-at`, at
    /// least seven days after the notice is logged, and the fee and level must each stay within 50%
    /// of their launch values. It replaces a pending notice, whatever that names.
    AnnounceParams {
        #[arg(long)]
        profit_fee_bps: Option<i64>,
        #[arg(long)]
        closeout_level_bps: Option<i64>,
        /// A TOML parameters file whose stress levels, move window and cooldown a later
        /// `set-params` applies, such as the file it will carry.
        #[arg(long)]
        stress_from: Option<PathBuf>,
        /// When the new values may apply from, in RFC 3339 (`2027-01-01T00:00:00Z`).
        #[arg(long)]
        effective_at: DateTime<Utc>,
    },
    /// Give notice of a listed market's new financing markup, in bps a year (from V49): an
    /// `upsert-market` carrying it applies only from `--effective-at`, at least seven days after the
    /// notice is logged. It replaces that market's pending markup notice.
    AnnounceMarkup {
        market: MarketId,
        #[arg(allow_negative_numbers = true)]
        markup_bps: i64,
        /// When the new markup may apply from, in RFC 3339 (`2027-01-01T00:00:00Z`).
        #[arg(long)]
        effective_at: DateTime<Utc>,
    },
    /// Rotate the operator key to the key at `address` (from V35). The old key signs nothing from
    /// seven days after the rotation is logged; sign later commands with `--rotated` and the new key.
    RotateOperator { address: AccountId },
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
    /// Adopt a close-spread ceiling for a market (from V40): the most its closes may pay, whatever
    /// the parameters do. A new market's ceiling applies from `--effective-at`, at least seven days
    /// after the command is logged; an adopted ceiling can only be lowered, never raised.
    AdoptCloseSpreadCeiling {
        /// The market the ceiling applies to.
        #[arg(short, long)]
        market: MarketId,
        /// The most a close pays, in bps of the index.
        #[arg(long)]
        max_close_spread_bps: i64,
        /// When the adoption takes effect, in RFC 3339 (`2027-01-01T00:00:00Z`), at least seven
        /// days after the command is logged.
        #[arg(long)]
        effective_at: DateTime<Utc>,
    },
    /// Encrypt a secrets bundle to a node's attested one-time key, after checking its quote against
    /// the release manifest, sign it, and deliver it to `[tee.sealing.secrets]`'s listener. Secrets
    /// come from a file, never the command line.
    DeliverSecrets(secrets::DeliverArgs),
    /// Print what a node's enclave attests it holds: the sealed and running bundle versions, and the
    /// secrets it lacks. No value leaves the enclave.
    SecretsStatus(secrets::PeerArgs),
}

/// A node's API: its `https://host:port` URL, and the hash its TLS key must have.
pub(crate) struct NodeApi {
    pub url: Uri,
    pub pin: [u8; 32],
}

impl AdminAction {
    /// The admin command this action sends.
    /// Error cases:
    /// - a spec or parameters file cannot be read, does not parse, or fails validation
    fn command(self) -> anyhow::Result<AdminCommand> {
        Ok(match self {
            AdminAction::ScheduleCorporateAction { file } => {
                let action: CorporateAction = input::read_toml(&file)?;
                action.validate().map_err(anyhow::Error::msg)?;
                AdminCommand::ScheduleCorporateAction(action)
            }
            AdminAction::UpsertMarket { file } => {
                let spec: MarketSpec = input::read_toml(&file)?;
                spec.validate().map_err(anyhow::Error::msg).with_context(|| file.display().to_string())?;
                AdminCommand::UpsertMarket(spec)
            }
            AdminAction::SetParams { file } => AdminCommand::SetParams(read_params(&file)?),
            AdminAction::SetBenchmarkRate { benchmark, annual_bps } => {
                AdminCommand::SetBenchmarkRate { benchmark, annual_bps }
            }
            AdminAction::SyncBenchmarks { .. } => bail!("publisher sync is asynchronous"),
            AdminAction::DeliverSecrets(_) | AdminAction::SecretsStatus(_) => bail!("secrets take no admin command"),
            AdminAction::SetStressFloor { level, market } => AdminCommand::SetStressFloor { market, level },
            AdminAction::SetReduceOnly { reduce_only, market } => AdminCommand::SetReduceOnly { market, reduce_only },
            AdminAction::AnnounceParams { profit_fee_bps, closeout_level_bps, stress_from, effective_at } => {
                let stress = stress_from.map(|file| read_params(&file).map(|params| StressSettings::of(&params)));
                AdminCommand::AnnounceParams(ParamsNotice {
                    profit_fee_bps,
                    closeout_level_bps,
                    stress: stress.transpose()?,
                    effective_at: effective_at.timestamp_millis(),
                })
            }
            AdminAction::AnnounceMarkup { market, markup_bps, effective_at } => {
                AdminCommand::AnnounceMarkup { market, markup_bps, effective_at: effective_at.timestamp_millis() }
            }
            AdminAction::RotateOperator { address } => AdminCommand::RotateOperator { address },
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
            AdminAction::AdoptCloseSpreadCeiling { market, max_close_spread_bps, effective_at } => {
                AdminCommand::AdoptCloseSpreadCeiling {
                    market,
                    max_close_spread_bps,
                    effective_at: effective_at.timestamp_millis(),
                }
            }
        })
    }
}

/// Runs what the command line asks for: a secrets delivery or status read, or an admin command.
/// Error cases:
/// - see `secrets::deliver`, `secrets::status`, and `run`
pub async fn dispatch(args: AdminArgs, env: KeyEnv) -> anyhow::Result<()> {
    match &args.action {
        AdminAction::DeliverSecrets(deliver) => {
            let cfg = NodeConfig::load(&args.config)?;
            crate::init_logging(&cfg.logging.level)?;
            let key = input::resolve_key(args.key_file.as_deref(), env)?;
            signing_operator(cfg.operator()?, &key, args.rotated, &args.config)?;
            secrets::deliver(&cfg, &key, deliver).await
        }
        AdminAction::SecretsStatus(peer) => {
            let cfg = NodeConfig::load(&args.config)?;
            crate::init_logging(&cfg.logging.level)?;
            secrets::status(&cfg, peer).await
        }
        _ => run(args, env).await.map(drop),
    }
}

/// Signs the command with the operator's next nonce and submits it, printing the signed request and
/// the events the engine produced. Returns the request it sent.
/// Error cases:
/// - the config does not load, or the key is missing, unreadable, malformed, or not the config's
///   operator key
/// - `--pin` is missing
/// - a spec or parameters file cannot be read, does not parse, or fails validation
/// - the node cannot be reached in time, or refuses the request (see `call`)
/// - the engine rejects the command
/// - for the publisher worker, its alert routes or outbox cannot start, or stop
pub async fn run(args: AdminArgs, env: KeyEnv) -> anyhow::Result<AdminRequest> {
    let cfg = NodeConfig::load(&args.config)?;
    if !matches!(args.action, AdminAction::SyncBenchmarks { .. }) {
        crate::init_logging(&cfg.logging.level)?;
        return run_configured(args, env, now_ms, cfg).await;
    }
    // The publisher worker is a process of its own, so its alerts need routing of their own, and an
    // outbox beside the node's rather than the node's.
    let Telemetry { alerts, .. } = init_telemetry(&cfg, |var| std::env::var(var).ok())?;
    let mut routing = worker_routing(alerts, &cfg).await?;
    let outcome = tokio::select! {
        result = run_configured(args, env, now_ms, cfg) => result,
        stopped = routing.stopped() => Err(stopped),
    };
    routing.drain(ALERT_DRAIN_GRACE).await;
    outcome
}

/// Starts the publisher worker's alert routing, with an outbox in the node's outbox directory's name
/// with `-benchmark-worker` added, which has to exist or be creatable on the worker's own host.
/// Error cases:
/// - `alerts.outbox.dir` names no directory
/// - routing cannot start; the durable outbox at the named path must be creatable and writable,
///   though a failure naming the path can also come from deeper routing setup
async fn worker_routing(alerts: Router, cfg: &NodeConfig) -> anyhow::Result<Routing> {
    let mut outbox = cfg.alerts.outbox.clone();
    let name = outbox.dir.file_name().context("alerts.outbox.dir names no directory")?.to_string_lossy();
    outbox.dir = outbox.dir.with_file_name(format!("{name}-benchmark-worker"));
    alerts.spawn(cfg.alerts.episode_quiet, &outbox).await.with_context(|| {
        format!(
            "starting the benchmark worker's alert routing; its durable outbox at {} must be creatable \
             and writable",
            outbox.dir.display()
        )
    })
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
    let key = input::resolve_key(args.key_file.as_deref(), env)?;
    let operator = signing_operator(cfg.operator()?, &key, args.rotated, &args.config)?;
    if let AdminAction::RotateOperator { address } = &args.action {
        cfg.refuse_rotation_to_test_key(*address)?;
    }
    let node = NodeApi { url: args.node, pin: args.pin.context("--pin is required for this command")? };
    if let AdminAction::SyncBenchmarks { watch } = args.action {
        return benchmark_feed::run(&cfg, &node, &key, &operator, clock, watch).await;
    }
    let command = args.action.command()?;
    let path = format!("/v1/accounts/{}/nonce", operator.address);
    let input::NonceView { next_nonce } = call(&node, Method::GET, &path, None).await?;
    let req = sign(&key, &operator, command, next_nonce, clock() + EXPIRES_IN_MS);
    println!("{}", serde_json::to_string(&req)?);
    let events = submit(&node, &req).await?;
    println!("{}", serde_json::to_string(&events)?);
    if let Some(Event::AdminRejected { reason }) = events.last() {
        bail!("the engine rejected the command: {reason}");
    }
    Ok(req)
}

/// The operator `key` signs as: the configured one, or with `rotated`, the key's own address on the
/// configured chain and vault. The node, which checks a signature against the key the log records,
/// refuses a rotated key the log does not name.
/// Error cases:
/// - without `rotated`, `key` is not the configured operator's
fn signing_operator(configured: Operator, key: &SigningKey, rotated: bool, config: &Path) -> anyhow::Result<Operator> {
    let address = eip712::address(key.verifying_key());
    if rotated {
        return Ok(Operator { address, ..configured });
    }
    if address != configured.address {
        bail!("the key is not the operator key {} in {}", configured.address, config.display());
    }
    Ok(configured)
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

/// Makes one HTTP/1.1 call to the node over TLS pinned to its key, and parses its JSON reply.
/// Error cases:
/// - the node URL is not an `https://host:port` URL: another scheme, or a path or query
/// - the node cannot be reached, its TLS key is not the pinned one, or it does not answer within
///   `CALL_TIMEOUT`
/// - a status other than 200, reported with the node's error body
/// - a reply that is not the expected JSON
/// The parameters in `file`, checked as the node runs the latest version would apply them.
fn read_params(file: &Path) -> anyhow::Result<ProtocolParams> {
    let params: ProtocolParams = input::read_toml(file)?;
    params
        .validate_for(ProtocolVersion::LATEST)
        .and_then(|()| params.check_closing_spread())
        .and_then(|()| params.check_notional_weighting())
        .map_err(anyhow::Error::msg)
        .with_context(|| file.display().to_string())?;
    Ok(params)
}

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
        let (host, port) = input::socket_target(authority);
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
