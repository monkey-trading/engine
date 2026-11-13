//! What a venue connection authenticates with: nothing for a public feed, a vendor's API key sent in
//! its login message, or, for CME Group, an OAuth 2.0 bearer token its WebSocket handshake carries.
//!
//! A token comes from the client-credentials grant (RFC 6749, section 4.4): a `POST` of
//! `grant_type=client_credentials` to the venue's token endpoint with the client ID and secret in
//! HTTP Basic auth, over TLS that ends in the enclave and trusts only the venue connections' roots.
//! Its `expires_in` sets its life, counted from before the request went out. Each connection opens on
//! a token fetched for it. Three quarters of the way through that token's life the next is fetched
//! while the connection streams, retrying with backoff, and once it is in hand a replacement opens on
//! it beside the connection (see `replace`); a connection still open when its token expires ends, and
//! the next opens on the token fetched ahead, if one is held, or fetches its own. The first request
//! that fails since one succeeded raises `venue_token_failed`, and the next that succeeds resolves
//! it; the requests that fail meanwhile are logged at `debug`.
//!
//! A refused request is named by its HTTP status and its OAuth error code, if RFC 6749 registers
//! it: nothing the endpoint writes is logged, as it could echo a credential in some encoding. A feed
//! also redacts every credential and token from what it logs and returns (`redact`), a backstop for
//! the adapters whose reasons still name a venue's fields, and none has a `Debug` that shows them.

use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::fmt;
use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use reqwest::{Client, Response, redirect};
use rustls::ClientConfig;
use serde::Deserialize;
use serde_json::Value;
use tokio::time::{Instant, sleep, sleep_until, timeout_at};
use tracing::{debug, info, warn};

use crate::config::ApiKey;
use crate::feeds::venue::setup::next_backoff;
use crate::feeds::venue::{Timing, VenueFeed};

/// The most of a token endpoint's answer that is read.
const MAX_ANSWER_BYTES: usize = 16 << 10;
/// The grant's form body.
const GRANT: &str = "grant_type=client_credentials";

/// The future that ends a connection on its token (see `OAuthClient::rollover`).
pub(super) type Rollover = Pin<Box<dyn Future<Output = TokenEnd> + Send>>;

/// What a venue connection authenticates with.
#[derive(Clone, Debug)]
pub enum Credentials {
    /// Nothing: the feed is public.
    Public,
    /// A vendor's API key, sent in its login message.
    ApiKey(ApiKey),
    /// A client of the venue's token endpoint, whose bearer token the WebSocket handshake carries.
    OAuth(OAuthClient),
}

/// A client of an OAuth token endpoint, holding the client ID and secret. `Debug` shows only the
/// endpoint.
#[derive(Clone)]
pub struct OAuthClient {
    http: Client,
    token_url: String,
    client_id: ApiKey,
    client_secret: ApiKey,
    /// The Basic credentials as a request carries them, `base64(id:secret)`, so they are redacted too.
    basic: ApiKey,
    /// Whether a request has failed since one last succeeded: `venue_token_failed`'s episode, shared
    /// by the clones a feed's connections use.
    failing: Arc<AtomicBool>,
}

/// A bearer token, when it is due for replacement, and when it expires. `Debug` hides the token.
#[derive(Debug)]
pub(super) struct Token {
    bearer: ApiKey,
    refresh_at: Instant,
    expires_at: Instant,
}

/// How the token a connection opened on ends it.
pub(super) enum TokenEnd {
    /// The next token is in hand.
    Fresh(Token),
    /// The token expired before a fresh one could be had.
    Expired,
}

/// A token endpoint's answer to a granted request (RFC 6749, section 5.1).
#[derive(Deserialize)]
struct Grant {
    access_token: String,
    token_type: String,
    expires_in: u64,
}

/// A token endpoint's answer to a refused request (RFC 6749, section 5.2): its error code (its
/// description, free text the endpoint writes, is not read).
#[derive(Deserialize)]
struct GrantError {
    error: String,
}

/// The error codes RFC 6749, section 5.2, registers for a refused token request.
const GRANT_ERRORS: [&str; 6] = [
    "invalid_request",
    "invalid_client",
    "invalid_grant",
    "unauthorized_client",
    "unsupported_grant_type",
    "invalid_scope",
];

impl fmt::Debug for OAuthClient {
    /// The endpoint, never a credential.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClient").field("token_url", &self.token_url).finish_non_exhaustive()
    }
}

impl Credentials {
    /// The API key a vendor's login message carries, if any.
    pub(super) fn api_key(&self) -> Option<&ApiKey> {
        match self {
            Credentials::ApiKey(key) => Some(key),
            Credentials::Public | Credentials::OAuth(_) => None,
        }
    }

    /// Every credential, as text a log line could carry.
    pub(super) fn secrets(&self) -> Vec<&str> {
        match self {
            Credentials::Public => Vec::new(),
            Credentials::ApiKey(key) => vec![key.expose()],
            Credentials::OAuth(client) => client.secrets().to_vec(),
        }
    }
}

impl OAuthClient {
    /// A client of the token endpoint at `token_url`, logging in as `client_id` with
    /// `client_secret`, over TLS with `tls`, each request bounded by `timeout`.
    /// Error cases:
    /// - the HTTP client cannot be built with `tls`
    pub fn new(
        token_url: &str,
        client_id: ApiKey,
        client_secret: ApiKey,
        tls: &ClientConfig,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        let http = Client::builder()
            .tls_backend_preconfigured(tls.clone())
            .no_proxy()
            .redirect(redirect::Policy::none())
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()
            .context("building the token endpoint's client")?;
        let basic = ApiKey::obtained(STANDARD.encode(format!("{}:{}", client_id.expose(), client_secret.expose())));
        let failing = Arc::default();
        Ok(OAuthClient { http, token_url: token_url.to_owned(), client_id, client_secret, basic, failing })
    }

    /// Fetches a token that lives at least `min_life`.
    /// Error cases:
    /// - the request fails or times out, or the endpoint refuses it (named by its status and its
    ///   OAuth error code, if one RFC 6749 registers)
    /// - the answer is over `MAX_ANSWER_BYTES`, does not parse, or is not a bearer token, or the
    ///   token is empty or not a header value, or lives less than `min_life` or out of range
    pub(super) async fn fetch(&self, min_life: Duration) -> anyhow::Result<Token> {
        let asked = Instant::now();
        let request = self
            .http
            .post(self.token_url.as_str())
            .basic_auth(self.client_id.expose(), Some(self.client_secret.expose()))
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(GRANT);
        let response =
            request.send().await.map_err(|e| anyhow::Error::new(e.without_url()).context("token request"))?;
        let status = response.status();
        let body = read_answer(response).await?;
        if !status.is_success() {
            // Named only by a registered code: the endpoint's text could echo a credential.
            let cause = match serde_json::from_slice::<GrantError>(&body) {
                Ok(refused) => GRANT_ERRORS
                    .iter()
                    .find(|code| **code == refused.error)
                    .copied()
                    .unwrap_or("an unregistered OAuth error"),
                Err(_) => "no OAuth error given",
            };
            bail!("token request refused ({status}): {cause}");
        }
        // A serde error can quote the answer, which holds the token, so only where it failed is kept.
        let grant: Grant = serde_json::from_slice(&body).map_err(|e| {
            anyhow!("token answer does not parse ({:?} at line {}, column {})", e.classify(), e.line(), e.column())
        })?;
        // Named without the answer's value, which an endpoint's mistake could make the token.
        ensure!(grant.token_type.eq_ignore_ascii_case("bearer"), "the token type is not bearer");
        let header = HeaderValue::from_str(&format!("Bearer {}", grant.access_token));
        ensure!(!grant.access_token.is_empty() && header.is_ok(), "the token is empty or not a header value");
        let life = Duration::from_secs(grant.expires_in);
        ensure!(life >= min_life, "a token living {life:?} is shorter than a connection's timeout, {min_life:?}");
        let expires_at = asked.checked_add(life).context("token lifetime out of range")?;
        let refresh_at = asked + (life - life / 4);
        Ok(Token { bearer: ApiKey::obtained(grant.access_token), refresh_at, expires_at })
    }

    /// `fetch`, for the connection `feed` of `venue`, alerting `venue_token_failed` at the first
    /// request that fails since one succeeded and resolving it at the next that succeeds. What it
    /// logs is redacted of the credentials and of `held`, the tokens the feed holds.
    /// Error cases:
    /// - as for `fetch`
    pub(super) async fn fetch_alerting(
        &self,
        min_life: Duration,
        venue: &'static str,
        feed: &str,
        held: &[&ApiKey],
    ) -> anyhow::Result<Token> {
        let fetched = self.fetch(min_life).await;
        match &fetched {
            Ok(_) if self.failing.swap(false, Ordering::Relaxed) => {
                info!(alert_resolved = "venue_token_failed", venue, feed, "bearer token granted");
            }
            Ok(_) => {}
            Err(error) => {
                let error = self.redact_with(&format!("{error:#}"), held);
                if self.failing.swap(true, Ordering::Relaxed) {
                    debug!(venue, feed, "bearer token request failed again: {error}");
                } else {
                    // The cause changes from request to request, so the connection alone is the key.
                    warn!(alert = "venue_token_failed", venue, feed, "bearer token request failed: {error}");
                }
            }
        }
        fetched
    }

    /// Ends a connection on `token`: from `from`, fetches the next, retrying with backoff from
    /// `timing.reconnect_min` to `timing.reconnect_max` (see `fetch_alerting`), and returns it; or
    /// returns `Expired` once `token` expires without one, `from` being later or not.
    fn rollover(self, token: &Token, from: Instant, timing: Timing, venue: &'static str, feed: String) -> Rollover {
        let (refresh_at, expires_at, held) = (from, token.expires_at, token.bearer.clone());
        Box::pin(async move {
            let retries = async {
                sleep_until(refresh_at).await;
                let mut backoff = timing.reconnect_min;
                loop {
                    if let Ok(token) = self.fetch_alerting(timing.timeout, venue, &feed, &[&held]).await {
                        return token;
                    }
                    sleep(backoff).await;
                    backoff = next_backoff(backoff, timing.reconnect_max);
                }
            };
            timeout_at(expires_at, retries).await.map_or(TokenEnd::Expired, TokenEnd::Fresh)
        })
    }

    /// The client ID, the secret, and the Basic credentials they make.
    fn secrets(&self) -> [&str; 3] {
        [self.client_id.expose(), self.client_secret.expose(), self.basic.expose()]
    }

    /// `text` with every credential and every one of `tokens` replaced.
    fn redact_with(&self, text: &str, tokens: &[&ApiKey]) -> String {
        let secrets: Vec<&str> = self.secrets().into_iter().chain(tokens.iter().map(|token| token.expose())).collect();
        redact(text, &secrets)
    }
}

impl Token {
    /// The token, for the handshakes that send it.
    pub(super) fn bearer(&self) -> &ApiKey {
        &self.bearer
    }

    /// When the token expires, counted from before it was requested.
    pub(super) fn expires_at(&self) -> Instant {
        self.expires_at
    }
}

impl VenueFeed {
    /// Takes the token the next connection opens on, for a venue that takes one: the one fetched
    /// ahead for it, or a new one.
    /// Error cases:
    /// - the token request fails (see `OAuthClient::fetch`)
    pub(super) async fn open_token(&mut self) -> anyhow::Result<()> {
        let Credentials::OAuth(client) = &self.credentials else { return Ok(()) };
        let token = match self.next_token.take() {
            Some(token) => token,
            None => {
                let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
                let held: Vec<&ApiKey> = self.token.iter().map(|token| &token.bearer).collect();
                client.fetch_alerting(self.timing.timeout, venue, feed, &held).await.context("bearer token")?
            }
        };
        self.token = Some(token);
        Ok(())
    }

    /// The bearer token the connection opens with, for a venue that takes one.
    pub(super) fn bearer(&self) -> Option<&str> {
        self.token.as_ref().map(|token| token.bearer.expose())
    }

    /// What ends the connection on its token (see `OAuthClient::rollover`), from three quarters of
    /// the token's life: never, for a venue that takes none.
    pub(super) fn rollover(&self) -> Rollover {
        self.rollover_from(self.token.as_ref().map(|token| token.refresh_at))
    }

    /// `rollover`, fetching the next token after `wait`, as once a replacement has failed.
    pub(super) fn retry_rollover(&self, wait: Duration) -> Rollover {
        self.rollover_from(Some(Instant::now() + wait))
    }

    /// `rollover`, fetching the next token from `from`.
    fn rollover_from(&self, from: Option<Instant>) -> Rollover {
        match (&self.credentials, &self.token, from) {
            (Credentials::OAuth(client), Some(token), Some(from)) => {
                client.clone().rollover(token, from, self.timing, self.conn.venue.name(), self.conn.url.clone())
            }
            _ => Box::pin(pending()),
        }
    }

    /// What ends the connection once its token expires, while a replacement opens on the next.
    pub(super) fn expiry(&self) -> Rollover {
        let Some(expires_at) = self.token.as_ref().map(|token| token.expires_at) else { return Box::pin(pending()) };
        Box::pin(async move {
            sleep_until(expires_at).await;
            TokenEnd::Expired
        })
    }

    /// Every credential and token this feed holds, as text a log line could carry.
    pub(super) fn secrets(&self) -> Vec<&str> {
        let tokens = [&self.token, &self.next_token].into_iter().flatten().map(|token| token.bearer.expose());
        self.credentials.secrets().into_iter().chain(tokens).collect()
    }
}

/// The backstop for text an adapter builds from a venue's fields (a CME connection logs none, see
/// `cme`): `text` with every one of `secrets` that is not empty replaced, as it is and as a JSON or Rust
/// string escapes it, the longest first: a secret inside a longer one would otherwise leave the rest
/// of the longer one. A message that does not parse, or a string escaped again, can carry a secret
/// escaped otherwise, so the escapes in `text` are decoded a layer at a time until none is left. If a
/// decoded layer shows a secret, the whole of `text` is replaced: the secret cannot be replaced where
/// it was written, and replacing it in the layer would need every secret there found whole, which a
/// shorter one inside a longer can prevent. So is a text whose escapes go deeper than
/// `MAX_ESCAPE_LAYERS` layers, which bounds the work a venue's text can cost.
pub(super) fn redact(text: &str, secrets: &[&str]) -> String {
    let forms: BTreeSet<String> =
        secrets.iter().filter(|secret| !secret.is_empty()).flat_map(|secret| escaped(secret)).collect();
    if forms.is_empty() {
        return text.to_owned();
    }
    let mut forms: Vec<String> = forms.into_iter().collect();
    forms.sort_by_key(|form| Reverse(form.len()));
    let mut layer = text.to_owned();
    for _ in 0..=MAX_ESCAPE_LAYERS {
        let decoded = unescaped(&layer);
        if decoded == layer {
            return forms.iter().fold(text.to_owned(), |text, form| text.replace(form.as_str(), REDACTED));
        }
        if forms.iter().any(|form| decoded.contains(form.as_str())) {
            return REDACTED.to_owned();
        }
        layer = decoded;
    }
    REDACTED.to_owned()
}

/// How many layers of escapes `redact` decodes before it replaces a text whole.
const MAX_ESCAPE_LAYERS: usize = 4;

/// What a secret, or a text that carries one under escapes, is replaced with.
const REDACTED: &str = "<redacted>";

/// `secret` as it is, as JSON writes it with and without `/` escaped, and as Rust's `Debug` does.
fn escaped(secret: &str) -> [String; 4] {
    let unquoted = |quoted: String| quoted[1..quoted.len() - 1].to_owned();
    let json = unquoted(Value::String(secret.to_owned()).to_string());
    [secret.to_owned(), json.replace('/', "\\/"), json, unquoted(format!("{secret:?}"))]
}

/// `text` with each JSON or Rust string escape in it decoded, once, wherever it stands. A backslash
/// that starts neither is kept.
fn unescaped(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('\\') {
        decoded.push_str(&rest[..at]);
        let (character, len) = escape(&rest[at + 1..]).unwrap_or(('\\', 0));
        decoded.push(character);
        rest = &rest[at + 1 + len..];
    }
    decoded.push_str(rest);
    decoded
}

/// The character an escape whose backslash `text` follows stands for, and how long the escape is
/// after its backslash: `\"`, `\\`, `\/`, `\'`, `\b`, `\f`, `\n`, `\r`, `\t`, `\0`, JSON's
/// `\uXXXX`, a surrogate pair of them included, and Rust's `\u{X}`.
fn escape(text: &str) -> Option<(char, usize)> {
    let hex = |digits: &str| {
        if !(1..=6).contains(&digits.len()) || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(digits, 16).ok()
    };
    let four = |at: usize| text.get(at..at + 4).and_then(hex);
    let character = match text.chars().next()? {
        '"' => '"',
        '\\' => '\\',
        '/' => '/',
        '\'' => '\'',
        'b' => '\u{8}',
        'f' => '\u{c}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        '0' => '\0',
        'u' if text[1..].starts_with('{') => {
            let end = text.find('}')?;
            return char::from_u32(hex(&text[2..end])?).map(|character| (character, end + 1));
        }
        'u' => {
            let code = four(1)?;
            if !(0xD800..0xDC00).contains(&code) {
                return char::from_u32(code).map(|character| (character, 5));
            }
            let low = four(7).filter(|low| text.get(5..7) == Some("\\u") && (0xDC00..0xE000).contains(low))?;
            let paired = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
            return char::from_u32(paired).map(|character| (character, 11));
        }
        _ => return None,
    };
    Some((character, 1))
}

/// The body of a token endpoint's answer, at most `MAX_ANSWER_BYTES`.
/// Error cases:
/// - reading fails, or the body is longer
async fn read_answer(mut response: Response) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) =
        response.chunk().await.map_err(|e| anyhow::Error::new(e.without_url()).context("token answer"))?
    {
        ensure!(chunk.len() <= MAX_ANSWER_BYTES - body.len(), "token answer over {MAX_ANSWER_BYTES} bytes");
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests;
