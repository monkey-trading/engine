use super::*;

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::config::{Venue, VenueConnection};
use crate::feeds::tests::cme_support::{Answer, CLIENT_ID, CLIENT_SECRET, CmeStandIn, key};
use crate::feeds::tests::fake::{fast, next_alert};
use crate::feeds::tests::rwa_support::TlsFront;
use crate::test_support::{capture_alerts, capture_alerts_and_resolutions};

/// A connection's timeout in these tests, which a token must outlive.
const TIMEOUT: Duration = Duration::from_secs(1);

#[tokio::test]
async fn a_token_is_fetched_with_the_client_credentials_grant_and_refreshed_a_quarter_before_expiry() {
    let tls = TlsFront::new();
    let stand_in = CmeStandIn::start(&tls, 1_799, Arc::new(|_, _| None)).await;
    let client = stand_in.client(&tls, TIMEOUT);
    let before = Instant::now();
    let token = client.fetch(TIMEOUT).await.unwrap();
    let after = Instant::now();

    let issued = stand_in.seen(|seen| seen.issued[0].0.clone());
    assert_eq!(token.bearer().expose(), issued);
    // The life counts from before the request went out, and the next is fetched at three quarters.
    let life = Duration::from_secs(1_799);
    assert!(token.expires_at >= before + life && token.expires_at <= after + life, "{token:?}");
    assert_eq!(token.expires_at - token.refresh_at, life / 4);
    let basic = format!("Basic {}", STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}")));
    let form = Some("application/x-www-form-urlencoded".to_owned());
    let request = (Some(basic), form, "grant_type=client_credentials".to_owned());
    assert_eq!(stand_in.seen(|seen| seen.requests.clone()), [request]);
    stand_in.stop();
}

#[tokio::test]
async fn a_refused_or_unusable_answer_is_an_error_naming_its_cause_without_the_token() {
    let tls = TlsFront::new();
    let stand_in = CmeStandIn::start(&tls, 1_799, Arc::new(|_, _| None)).await;
    let client = stand_in.client(&tls, TIMEOUT);
    let grant = |token: &str, kind: &str, life: Value| {
        Answer::Canned(
            StatusCode::OK,
            json!({ "access_token": token, "token_type": kind, "expires_in": life }).to_string(),
        )
    };
    // Named by its status and registered code alone: the endpoint's own text is not read.
    let refused = |error: &str| json!({ "error_description": "VENDOR-TEXT tok-secret-1", "error": error, "error_uri": "VENDOR-TEXT" });
    let cases = [
        (
            Answer::Canned(StatusCode::UNAUTHORIZED, refused("invalid_client").to_string()),
            "token request refused (401 Unauthorized): invalid_client",
        ),
        (
            Answer::Canned(StatusCode::BAD_REQUEST, refused("VENDOR-TEXT").to_string()),
            "token request refused (400 Bad Request): an unregistered OAuth error",
        ),
        (Answer::Canned(StatusCode::BAD_GATEWAY, "VENDOR-TEXT".into()), "(502 Bad Gateway): no OAuth error given"),
        (grant("tok-secret-1", "mac", json!(1_799)), "the token type is not bearer"),
        (grant("", "bearer", json!(1_799)), "the token is empty or not a header value"),
        (grant("tok-secret-1\n", "bearer", json!(1_799)), "the token is empty or not a header value"),
        (grant("tok-secret-1", "Bearer", json!(0)), "shorter than a connection's timeout"),
        (grant("tok-secret-1", "bearer", json!(u64::MAX)), "token lifetime out of range"),
        (grant("tok-secret-1", "bearer", json!("1799")), "token answer does not parse"),
        // CME's documented example, whose trailing comma no JSON parser takes.
        (
            Answer::Canned(
                StatusCode::OK,
                r#"{"access_token":"tok-secret-1","token_type":"bearer","expires_in":1799,}"#.into(),
            ),
            "token answer does not parse",
        ),
        (Answer::Canned(StatusCode::OK, "x".repeat(MAX_ANSWER_BYTES + 1)), "token answer over 16384 bytes"),
    ];
    for (answer, expected) in cases {
        stand_in.answer(answer);
        let err = format!("{:#}", client.fetch(TIMEOUT).await.unwrap_err());
        assert!(err.contains(expected), "expected {expected:?}, got {err}");
        assert!(!err.contains("tok-secret-1") && !err.contains("VENDOR-TEXT"), "{err}");
    }
    // A token type is case-insensitive (RFC 6749, section 5.1).
    stand_in.answer(grant("tok-secret-1", "Bearer", json!(1_799)));
    assert_eq!(client.fetch(TIMEOUT).await.unwrap().bearer().expose(), "tok-secret-1");
    stand_in.stop();
}

#[tokio::test]
async fn an_unreachable_endpoint_fails_within_the_timeout() {
    let tls = TlsFront::new();
    // A listener that accepts and never answers.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://{}/as/token.oauth2", silent.local_addr().unwrap());
    let client = OAuthClient::new(&url, key(CLIENT_ID), key(CLIENT_SECRET), &tls.client, TIMEOUT).unwrap();
    let started = Instant::now();
    let err = format!("{:#}", client.fetch(TIMEOUT).await.unwrap_err());
    assert!(err.contains("token request"), "{err}");
    assert!(started.elapsed() < TIMEOUT * 3, "took {:?}", started.elapsed());
}

#[tokio::test]
async fn debug_output_names_no_credential_or_token() {
    let tls = TlsFront::new();
    let stand_in = CmeStandIn::start(&tls, 1_799, Arc::new(|_, _| None)).await;
    let client = stand_in.client(&tls, TIMEOUT);
    let token = client.fetch(TIMEOUT).await.unwrap();
    let basic = STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"));
    let shown =
        format!("{:?} {token:?} {:?}", Credentials::OAuth(client.clone()), Credentials::ApiKey(key(CLIENT_SECRET)));
    for secret in [CLIENT_ID, CLIENT_SECRET, basic.as_str(), token.bearer().expose()] {
        assert!(!shown.contains(secret), "{shown}");
    }
    assert!(shown.contains(&stand_in.token_url), "{shown}");
    // What a feed redacts: every credential, in the forms a request carries them.
    let credentials = Credentials::OAuth(client);
    let mut secrets = credentials.secrets();
    secrets.sort_unstable();
    let mut expected = vec![CLIENT_ID, CLIENT_SECRET, basic.as_str()];
    expected.sort_unstable();
    assert_eq!(secrets, expected);
    stand_in.stop();
}

#[test]
fn a_secret_escaped_in_a_message_that_does_not_parse_or_escaped_again_is_redacted() {
    let secrets = ["tb/4K+v", r#"pw"Z9\k&m"#, "ab\u{1F600}Qx"];
    let echoes = [
        // A JSON escape of a code point, in a message that does not parse.
        r#"{"header":{"messageType":"LOGINERR"},"payload":{"token":"tb\u002f4K+v"},}"#,
        // A JSON escape escaped again inside a message that parses.
        r#"{"token":"tb\\u002f4K+v"}"#,
        // JSON escapes quoted again by `Debug`, one of them of a code point.
        r#"closed: "{\"token\":\"tb\\/4K+v\"}""#,
        r#"refused "pw\\\"Z9\\\\k\\u0026m""#,
        // Rust's escape of a code point, and a JSON surrogate pair.
        r"token tb\u{2f}4K+v",
        r"id ab\ud83d\ude00Qx",
    ];
    for echo in echoes {
        let redacted = redact(echo, &secrets);
        for part in ["4K", "Z9", "Qx"] {
            assert!(!redacted.contains(part), "{part} in {redacted}");
        }
    }
    // A secret inside a longer one, the longer written with an escape: the text is replaced whole.
    let secrets = ["account42", "account42-secret"];
    let echo = r#"{"password":"account42-\u0073ecret",}"#;
    assert_eq!(redact(echo, &secrets), "<redacted>");
    // A secret written as it is, in text with no escape, is replaced where it stands; text that
    // carries no secret keeps its escapes, and a backslash that starts none stays.
    assert_eq!(redact("id account42-secret, account42", &secrets), "id <redacted>, <redacted>");
    let plain = r#"{"price":"1\/2"} \q \u12 \u{zz}"#;
    assert_eq!(redact(plain, &secrets), plain);
    // A text escaped deeper than the layers decoded is replaced whole; with no secret held, none is.
    let deep = format!("x{}y", "\\".repeat(64));
    assert_eq!(redact(&deep, &secrets), "<redacted>");
    assert_eq!(redact(&deep, &[]), deep);
}

#[tokio::test]
async fn a_refused_message_is_excerpted_only_from_a_venue_that_holds_no_credential() {
    let (_guard, mut alerts) = capture_alerts();
    let tls = TlsFront::new();
    let url = "https://auth.cmegroup.com/as/token.oauth2";
    let client = OAuthClient::new(url, key(CLIENT_ID), key(CLIENT_SECRET), &tls.client, TIMEOUT).unwrap();
    let text = r#"{"header":{"messageType":"LOGINERR"},"payload":{"echo":"VENDOR-TEXT"}}"#;
    let hashed = format!("a {}-byte message, SHA-256 ", text.len());
    for (credentials, excerpted) in [(Credentials::Public, true), (Credentials::OAuth(client), false)] {
        let conn = VenueConnection {
            venue: Venue::Cme,
            url: "wss://markets.api.cmegroup.com/marketdatastream/v1".into(),
            api_key_env: None,
            api_key_secret: None,
            oauth: None,
            symbols: Vec::new(),
        };
        let mut feed = VenueFeed::new(conn, credentials, fast(TIMEOUT), tls.client.clone(), mpsc::channel(1).0);
        feed.refuse("login refused", text);
        let alert = next_alert(&mut alerts).await;
        let message = alert["message"].as_str();
        assert_eq!(alert["alert"].as_str(), "venue_message_refused");
        assert_eq!((message.contains("VENDOR-TEXT"), message.contains(&hashed)), (excerpted, !excerpted), "{message}");
    }
}

#[tokio::test]
async fn a_rollover_fetches_the_next_token_or_ends_at_expiry_retrying_meanwhile() {
    let tls = TlsFront::new();
    let stand_in = CmeStandIn::start(&tls, 3, Arc::new(|_, _| None)).await;
    let client = stand_in.client(&tls, TIMEOUT);
    let timing = Timing {
        interval: Duration::from_millis(10),
        timeout: TIMEOUT,
        reconnect_min: Duration::from_millis(100),
        reconnect_max: Duration::from_millis(200),
        stale_after: Duration::from_secs(1),
        ping_interval: Duration::from_millis(500),
        max_delay: Duration::from_secs(2),
    };
    // Due at a token's three quarters, not before.
    let token = client.fetch(TIMEOUT).await.unwrap();
    let rollover = client.clone().rollover(&token, token.refresh_at, timing, "cme", "test".into());
    let TokenEnd::Fresh(next) = rollover.await else { panic!("no fresh token") };
    assert!(Instant::now() >= token.refresh_at && Instant::now() < token.expires_at);
    assert_ne!(next.bearer().expose(), token.bearer().expose());

    // Refused from then on: it retries with backoff, and ends once the token expires.
    stand_in.answer(Answer::Refuse);
    let requests = stand_in.seen(|seen| seen.requests.len());
    let TokenEnd::Expired = client.clone().rollover(&next, next.refresh_at, timing, "cme", "test".into()).await else {
        panic!("a refused refresh gave a token")
    };
    assert!(Instant::now() >= next.expires_at);
    // The 750 ms from its refresh to its expiry hold several tries, 100 to 200 ms apart.
    let retries = stand_in.seen(|seen| seen.requests.len()) - requests;
    assert!((2..=10).contains(&retries), "{retries} tries");

    // Due only after the token expires, as a retry's backoff can make it: it ends at the expiry.
    let from = Instant::now();
    let token = Token { bearer: key("held"), refresh_at: from, expires_at: from + Duration::from_millis(300) };
    let late = client.clone().rollover(&token, from + Duration::from_secs(30), timing, "cme", "test".into());
    let ended = tokio::time::timeout(Duration::from_secs(5), late).await.expect("ended at the expiry");
    assert!(matches!(ended, TokenEnd::Expired));
    assert!(Instant::now() >= token.expires_at);
    stand_in.stop();
}

#[tokio::test]
async fn a_credential_or_token_echoed_back_never_reaches_an_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let tls = TlsFront::new();
    let stand_in = CmeStandIn::start(&tls, 1_799, Arc::new(|_, _| None)).await;
    let client = stand_in.client(&tls, TIMEOUT);
    let (token, next) = (client.fetch(TIMEOUT).await.unwrap(), client.fetch(TIMEOUT).await.unwrap());
    let basic = STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"));
    let secrets =
        [CLIENT_ID, CLIENT_SECRET, basic.as_str(), token.bearer().expose(), next.bearer().expose()].map(str::to_owned);
    let echoed = secrets.join(" ");
    let conn = VenueConnection {
        venue: Venue::Cme,
        url: stand_in.stream_url.clone(),
        api_key_env: None,
        api_key_secret: None,
        oauth: None,
        symbols: Vec::new(),
    };
    let timing = fast(TIMEOUT);
    let mut feed = VenueFeed::new(conn, Credentials::OAuth(client), timing, tls.client.clone(), mpsc::channel(1).0);
    // The token the connection opened on, and the next, fetched ahead.
    (feed.token, feed.next_token) = (Some(token), Some(next));

    feed.refuse(&format!("login refused for {echoed}"), &echoed);
    feed.dropped(&anyhow!("handshake with {echoed} failed"), Duration::from_secs(1));
    for name in ["venue_message_refused", "venue_feed_down"] {
        let alert = next_alert(&mut alerts).await;
        let message = alert["message"].as_str();
        assert_eq!(alert["alert"].as_str(), name);
        assert!(secrets.iter().all(|secret| !message.contains(secret.as_str())), "{alert:?}");
        assert!(message.matches("<redacted>").count() >= secrets.len(), "{alert:?}");
    }
    stand_in.stop();
}

#[test]
fn redaction_replaces_the_longest_secret_first() {
    // An ID inside its password, and a token inside a longer one, in either order.
    let cases: [(&[&str], &str); 2] = [
        (&["account42", "account42-secret"], "refused account42-secret for account42"),
        (&["tok-1-9", "tok-1"], "tok-1 then tok-1-9"),
    ];
    for (secrets, text) in cases {
        for order in [secrets.to_vec(), secrets.iter().rev().copied().collect()] {
            let redacted = redact(text, &order);
            assert_eq!(redacted.matches("<redacted>").count(), 2, "{redacted}");
            assert!(!redacted.contains("secret") && !redacted.contains("-9"), "{redacted}");
        }
    }
    assert_eq!(redact("no key", &["", "absent"]), "no key");
}

#[tokio::test]
async fn a_failed_token_request_alerts_once_and_a_granted_one_resolves_it() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let tls = TlsFront::new();
    let stand_in = CmeStandIn::start(&tls, 1_799, Arc::new(|_, _| None)).await;
    let client = stand_in.client(&tls, TIMEOUT);
    // The endpoint refuses, echoing the credentials and the token the feed holds.
    let held = key("tok-held-5150");
    let echoed = json!({ "error": "invalid_client", "error_description": format!("{CLIENT_SECRET} tok-held-5150") });
    stand_in.answer(Answer::Canned(StatusCode::UNAUTHORIZED, echoed.to_string()));
    for _ in 0..3 {
        assert!(client.fetch_alerting(TIMEOUT, "cme", "test-feed", &[&held]).await.is_err());
    }
    let alert = next_alert(&mut alerts).await;
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("venue_token_failed", "WARN"));
    assert_eq!((alert["venue"].as_str(), alert["feed"].as_str()), ("cme", "test-feed"));
    // The alert names the answer by its status and code, not by the text that echoes the
    // credentials and the held token.
    let message = alert["message"].as_str();
    assert!(message.ends_with("token request refused (401 Unauthorized): invalid_client"), "{message}");
    assert!(!message.contains(CLIENT_SECRET) && !message.contains("tok-held-5150"), "{message}");
    assert!(alerts.try_recv().is_err(), "the same episode alerted again");

    // A clone, as the next connection's rollover holds, shares the episode and ends it.
    stand_in.answer(Answer::Grant(1_799));
    client.clone().fetch_alerting(TIMEOUT, "cme", "test-feed", &[]).await.unwrap();
    let end = next_alert(&mut resolved).await;
    assert_eq!((end["alert_resolved"].as_str(), end["feed"].as_str()), ("venue_token_failed", "test-feed"));
    client.fetch_alerting(TIMEOUT, "cme", "test-feed", &[]).await.unwrap();
    assert!(resolved.try_recv().is_err(), "a success with nothing failing resolved again");
    stand_in.stop();
}

#[tokio::test]
async fn an_escaped_credential_or_token_echoed_back_never_reaches_an_alert() {
    let (_guard, mut alerts) = capture_alerts();
    let tls = TlsFront::new();
    // Credentials and a token holding characters JSON escapes.
    let (id, secret, bearer) = ("idx/7Q", r#"pw"Z9\k&m"#, "tb/4K+v");
    let url = "https://auth.cmegroup.com/as/token.oauth2";
    let client = OAuthClient::new(url, key(id), key(secret), &tls.client, TIMEOUT).unwrap();
    let basic = client.basic.expose().to_owned();
    let conn = VenueConnection {
        venue: Venue::Cme,
        url: "wss://markets.api.cmegroup.com/marketdatastream/v1".into(),
        api_key_env: None,
        api_key_secret: None,
        oauth: None,
        symbols: Vec::new(),
    };
    let mut feed =
        VenueFeed::new(conn, Credentials::OAuth(client), fast(TIMEOUT), tls.client.clone(), mpsc::channel(1).0);
    let now = Instant::now();
    feed.token = Some(Token { bearer: key(bearer), refresh_at: now, expires_at: now });

    // Echoed escaped every way JSON allows: `/` as `\/`, `"` and `\` escaped, and `&` as a `\u`
    // escape of its code point.
    let ampersand = ["\\", "u0026"].concat();
    let echoed = json!({ "header": { "messageType": "LOGINERR" } }).to_string().replace(
        "}}",
        &format!(
            r#"}},"payload":{{"token":"tb\/4K+v","id":"idx\/7Q","password":"pw\"Z9\\k{ampersand}m","basic":"{}"}}}}"#,
            basic.replace('/', r"\/")
        ),
    );
    assert!(echoed.contains(&format!("k{ampersand}m")), "{echoed}");
    assert!(serde_json::from_str::<Value>(&echoed).is_ok(), "{echoed}");
    feed.refuse("login refused", &echoed);
    // And quoted as Rust's `Debug` writes a string, as an error can.
    feed.dropped(&anyhow!("closed: {secret:?} {bearer:?} {id:?}"), Duration::from_secs(1));
    for name in ["venue_message_refused", "venue_feed_down"] {
        let alert = next_alert(&mut alerts).await;
        let message = alert["message"].as_str();
        assert_eq!(alert["alert"].as_str(), name);
        for part in ["7Q", "Z9", "4K", &basic[..8]] {
            assert!(!message.contains(part), "{part} in {message}");
        }
    }
}
