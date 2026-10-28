//! Signed user requests and real HTTPS/WebSocket API traffic for the local scenario.

use std::collections::VecDeque;
use std::time::Duration;

use alloy::primitives::Address;
use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::access::{EligibilityClass, EligibilityRequest};
use engine_core::auth::RequestAuth;
use engine_core::command::{OrderKind, OrderRequest, WithdrawalRequest};
use engine_core::eip712::{self, Eip712Domain};
use engine_core::event::Event;
use engine_core::types::{AccountId, Side};
use futures_util::{SinkExt, StreamExt};
use k256::ecdsa::SigningKey;
use serde::Serialize;
use serde_json::Value;
use tokio::time::{sleep, timeout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::{client::generate_key, derive_accept_key};
use tokio_tungstenite::tungstenite::protocol::Role;

use crate::clock::now_ms;
use crate::sequencer::SignedWithdrawal;
use crate::test_support::LOADED_WAIT;

pub(super) struct User {
    pub account: AccountId,
    key: SigningKey,
}

#[derive(Clone)]
pub(super) struct Client {
    http: reqwest::Client,
    base: String,
    domain: Eip712Domain,
}

pub(super) struct Stream {
    socket: WebSocketStream<reqwest::Upgraded>,
    pending: VecDeque<SignedWithdrawal>,
}

impl User {
    /// Creates an explicitly public deterministic local signing identity.
    /// Error cases:
    /// - the repeated test byte is not a valid secp256k1 scalar
    pub fn new(byte: u8) -> anyhow::Result<Self> {
        let key = SigningKey::from_slice(&[byte; 32])?;
        Ok(Self { account: eip712::address(key.verifying_key()), key })
    }

    /// The corresponding token recipient/depositor address.
    pub fn address(&self) -> Address {
        Address::from(self.account.0)
    }
}

impl Client {
    /// Connects only to the local node's actual TLS API with its ephemeral self-signed test cert.
    /// Error cases:
    /// - HTTP client construction fails
    pub fn new(port: u16, vault: Address) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .http1_only()
                .tls_danger_accept_invalid_certs(true)
                .timeout(LOADED_WAIT)
                .build()?,
            base: format!("https://127.0.0.1:{port}"),
            domain: eip712::domain(31337, AccountId(vault.into())),
        })
    }

    /// Reads JSON through the normal TLS/rate-limited API.
    /// Error cases:
    /// - transport, HTTP status or JSON decoding fails
    pub async fn get(&self, path: &str) -> anyhow::Result<Value> {
        Ok(self.http.get(format!("{}{path}", self.base)).send().await?.error_for_status()?.json().await?)
    }

    /// Posts an actually signed request and returns the sequenced public events.
    /// Error cases:
    /// - transport, response status, or event decoding fails
    pub async fn post(&self, path: &str, body: &impl Serialize) -> anyhow::Result<Vec<Event>> {
        let response = self.http.post(format!("{}{path}", self.base)).json(body).send().await?;
        let status = response.status();
        let value: Value = response.json().await?;
        ensure!(status.is_success(), "local API {path} returned {status}: {value}");
        Ok(serde_json::from_value(value)?)
    }

    /// Reads the next request nonce, which differs from the Vault's payout nonce.
    /// Error cases:
    /// - the account/nonce response is unavailable or malformed
    pub async fn nonce(&self, user: &User) -> anyhow::Result<u64> {
        self.get(&format!("/v1/accounts/{}/nonce", user.account)).await?["next_nonce"]
            .as_u64()
            .context("account request nonce")
    }

    /// Signs a short-lived withdrawal under the actual chain and deployed Vault domain.
    /// Error cases:
    /// - nonce lookup or decimal amount parsing fails
    pub async fn withdrawal(&self, user: &User, amount: &str) -> anyhow::Result<WithdrawalRequest> {
        let nonce = self.nonce(user).await?;
        let amount = amount.parse()?;
        let expires_at = now_ms() + 120_000;
        let hash = eip712::withdrawal_hash(&self.domain, user.account, amount, nonce, expires_at);
        Ok(WithdrawalRequest {
            account: user.account,
            amount,
            auth: Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&user.key, &hash) }),
        })
    }

    /// Records the user's signed eligibility declaration, retrying only the pre-log refusal that
    /// its deposit is not yet pending; the deposit credits once the declaration is recorded.
    /// Error cases:
    /// - any other refusal, or the deposit never reaches the node
    pub async fn declare(&self, user: &User) -> anyhow::Result<()> {
        let class = EligibilityClass::Professional;
        for _ in 0..50 {
            let nonce = self.nonce(user).await?;
            let expires_at = now_ms() + 120_000;
            let request = EligibilityRequest {
                account: user.account,
                class,
                auth: RequestAuth {
                    nonce,
                    expires_at,
                    signature: eip712::sign(
                        &user.key,
                        &eip712::eligibility_hash(&self.domain, user.account, class, nonce, expires_at),
                    ),
                },
            };
            match self.post("/v1/eligibility", &request).await {
                Ok(events) => {
                    ensure!(
                        events.iter().any(|event| matches!(event, Event::EligibilityAttested { .. })),
                        "declaration refused: {events:?}"
                    );
                    return Ok(());
                }
                // A declaration ahead of its deposit is refused before the log and spends nothing.
                Err(error) if error.to_string().contains("403") => sleep(Duration::from_millis(100)).await,
                Err(error) => return Err(error),
            }
        }
        anyhow::bail!("declaration stayed refused: the deposit never reached the node");
    }

    /// Retries only explicit pre-sequencing 503 refusals while finalized custody catches up.
    /// Error cases:
    /// - any ambiguous transport failure, another status, decoding, or the deadline fails
    pub async fn request_withdrawal(&self, request: &WithdrawalRequest) -> anyhow::Result<Vec<Event>> {
        timeout(LOADED_WAIT, async {
            loop {
                let response = self.http.post(format!("{}/v1/withdrawals", self.base)).json(request).send().await?;
                let status = response.status();
                if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                    sleep(Duration::from_millis(100)).await;
                    continue;
                }
                let body: Value = response.json().await?;
                ensure!(status.is_success(), "withdrawal API returned {status}: {body}");
                return Ok(serde_json::from_value(body)?);
            }
        })
        .await
        .context("withdrawal waited for finalized healthy custody")?
    }

    /// Places a market order signed by the account's real test key at the API's current mark.
    /// Error cases:
    /// - price/nonce lookup, signing input, or submission fails
    pub async fn order(&self, user: &User, side: Side, qty: &str) -> anyhow::Result<Vec<Event>> {
        let nonce = self.nonce(user).await?;
        let mut order = OrderRequest {
            account: user.account,
            client_id: nonce,
            market: 1,
            side,
            qty: qty.parse()?,
            kind: OrderKind::Market { quoted: self.mark().await?, max_slippage_bps: None },
            reduce_only: false,
            auth: None,
        };
        let expires_at = now_ms() + 120_000;
        let hash = eip712::order_hash(&self.domain, &order, nonce, expires_at);
        order.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&user.key, &hash) });
        self.post("/v1/orders", &order).await
    }

    /// Reads the controlled market's actual engine mark.
    /// Error cases:
    /// - the market has not been listed/priced or the response is malformed
    pub async fn mark(&self) -> anyhow::Result<Fixed> {
        let markets = self.get("/v1/markets").await?;
        let value = markets
            .as_array()
            .context("market list")?
            .iter()
            .find(|market| market["id"] == 1)
            .context("scenario market listed")?;
        Ok(value["mark"].as_str().context("scenario market priced")?.parse()?)
    }

    /// Waits on an observable mark, never on a guessed feed propagation sleep.
    /// Error cases:
    /// - the expected mark is absent past its deadline
    pub async fn wait_mark(&self, expected: i64) -> anyhow::Result<()> {
        timeout(LOADED_WAIT, async {
            while self.mark().await? != Fixed::from_int(expected) {
                sleep(Duration::from_millis(80)).await;
            }
            anyhow::Ok(())
        })
        .await
        .context("controlled quote reached the engine")??;
        Ok(())
    }

    /// Waits for custody or trading to produce an exact engine account balance.
    /// Error cases:
    /// - the expected balance is absent past its deadline
    pub async fn wait_balance(&self, user: &User, expected: &str) -> anyhow::Result<()> {
        let expected: Fixed = expected.parse()?;
        timeout(LOADED_WAIT, async {
            loop {
                if let Ok(value) = self.get(&format!("/v1/accounts/{}", user.account)).await
                    && value["balance"].as_str().and_then(|text| text.parse::<Fixed>().ok()) == Some(expected)
                {
                    return;
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .with_context(|| format!("account {} balance did not reach {expected}", user.account))?;
        Ok(())
    }

    /// Waits for every deposit of an account to be final, so payout capacity counts its principal.
    /// Error cases:
    /// - the principal stays unfinalised past its deadline
    pub async fn wait_finalised(&self, user: &User) -> anyhow::Result<()> {
        timeout(LOADED_WAIT, async {
            loop {
                if let Ok(value) = self.get(&format!("/v1/accounts/{}", user.account)).await {
                    let unfinalised =
                        value["unfinalised_deposits"].as_str().and_then(|text| text.parse::<Fixed>().ok());
                    if unfinalised.is_none() || unfinalised == Some(Fixed::ZERO) {
                        return;
                    }
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .with_context(|| format!("account {} deposits did not finalise", user.account))?;
        Ok(())
    }

    /// Waits for custody to credit house capital to an exact house cash balance.
    /// Error cases:
    /// - the expected house cash is absent past its deadline
    pub async fn wait_house_cash(&self, expected: &str) -> anyhow::Result<()> {
        let expected: Fixed = expected.parse()?;
        timeout(LOADED_WAIT, async {
            loop {
                if let Ok(value) = self.get("/v1/house").await
                    && value["cash"].as_str().and_then(|text| text.parse::<Fixed>().ok()) == Some(expected)
                {
                    return;
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .with_context(|| format!("house cash did not reach {expected}"))?;
        Ok(())
    }

    /// Upgrades the actual HTTPS endpoint to a resumable WebSocket and checks its accept key.
    /// Error cases:
    /// - HTTP upgrade/handshake validation or TLS fails
    pub async fn stream(&self) -> anyhow::Result<Stream> {
        let key = generate_key();
        let response = self
            .http
            .get(format!("{}/v1/stream?from=0", self.base))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", &key)
            .send()
            .await?;
        ensure!(response.status() == reqwest::StatusCode::SWITCHING_PROTOCOLS, "actual stream did not upgrade");
        ensure!(
            response.headers().get("sec-websocket-accept").and_then(|value| value.to_str().ok())
                == Some(derive_accept_key(key.as_bytes()).as_str()),
            "WebSocket accept key mismatch"
        );
        let socket = WebSocketStream::from_raw_socket(response.upgrade().await?, Role::Client, None).await;
        Ok(Stream { socket, pending: VecDeque::new() })
    }
}

impl Stream {
    /// Receives an actual public authorization while retaining other accounts' messages in order.
    /// Error cases:
    /// - the stream ends/fails or the expected payout is absent past its deadline
    pub async fn payout(&mut self, account: AccountId, amount: i64) -> anyhow::Result<SignedWithdrawal> {
        timeout(LOADED_WAIT, async {
            loop {
                if let Some(index) = self
                    .pending
                    .iter()
                    .position(|payout| payout.account == account && payout.amount == Fixed::from_int(amount))
                {
                    return self.pending.remove(index).context("located pending payout");
                }
                match self.socket.next().await.context("stream closed before payout")?? {
                    Message::Text(text) => {
                        let value: Value = serde_json::from_str(&text)?;
                        if let Some(payout) = payout_frame(value)? {
                            self.pending.push_back(payout);
                        }
                    }
                    Message::Ping(data) => self.socket.send(Message::Pong(data)).await?,
                    Message::Close(_) => anyhow::bail!("stream closed before payout"),
                    _ => {}
                }
            }
        })
        .await
        .context("actual public stream payout deadline")?
    }
}

/// Removes only the stream envelope before decoding the strict withdrawal payload.
/// Error cases:
/// - a withdrawal frame lacks a valid offset or contains an invalid payload
fn payout_frame(mut frame: Value) -> anyhow::Result<Option<SignedWithdrawal>> {
    if frame["type"] != "withdrawal" {
        return Ok(None);
    }
    let object = frame.as_object_mut().context("stream frame object")?;
    let offset = object.remove("offset").context("withdrawal frame offset")?;
    let _: crate::log::Offset = serde_json::from_value(offset)?;
    object.remove("type");
    Ok(Some(serde_json::from_value(frame)?))
}

#[test]
fn actual_stream_envelope_decodes_without_weakening_the_withdrawal_payload() {
    let (payout, _) = crate::withdrawals::tests::vector();
    let message = crate::sequencer::StreamMsg::new(
        crate::log::Offset::start_of(5),
        crate::log::LogEntry::Withdrawal(payout.clone()),
    );
    let frame = serde_json::to_value(message).unwrap();
    assert_eq!(payout_frame(frame.clone()).unwrap(), Some(payout));
    let mut unknown = frame.clone();
    unknown["unexpected_payload_field"] = true.into();
    assert!(payout_frame(unknown).is_err());
    let mut missing = frame;
    missing.as_object_mut().unwrap().remove("offset");
    assert!(payout_frame(missing).is_err());
}
