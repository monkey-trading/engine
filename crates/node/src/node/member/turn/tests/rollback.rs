//! A member resolves a stream's `operator_log_rollback` only once a stint has repeated its copy's
//! whole tail, as the standby does: a heartbeat alone shows the operator's head, not its history.

use super::episodes::next;
use super::*;
use crate::test_support::capture_alerts_and_resolutions;

/// After a heartbeat behind the copy pages `operator_log_rollback`, a stint that hears a heartbeat
/// at the copy's head and then nothing leaves the episode open, and one that repeats the copy's tail
/// ends it, naming the operator the page did.
#[tokio::test]
async fn a_rollback_resolves_only_once_a_stint_repeats_the_copys_tail() {
    for repeats in [false, true] {
        let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
        let fixture = Fixture::start(&format!("member-rollback-{repeats}"), true).await;
        let operator = rolled_back_then(&fixture.log, repeats).await;
        fixture.registry.name(&operator.host);
        fixture.time.set(FIRST + 1);
        let mut member = fixture.member();
        let drive = async {
            let paged = next(&mut alerts, "operator_log_rollback").await;
            assert_eq!(paged["operator"], format!("https://{}", operator.host));
            if repeats {
                let ended = next(&mut resolved, "operator_log_rollback").await;
                assert_eq!(ended["operator"], paged["operator"]);
            } else {
                // The second stint ended with its heartbeat alone, and a third began.
                operator.wait_for_connections(3).await;
                let ended = std::iter::from_fn(|| resolved.try_recv().ok())
                    .any(|line| line["alert_resolved"] == "operator_log_rollback");
                assert!(!ended, "a heartbeat alone resolved the rollback");
            }
        };
        tokio::select! {
            _ = member.until_turn() => panic!("the member stopped following"),
            () = drive => {}
        }
    }
}

/// A stand-in operator whose first connection sends a signed heartbeat naming a head behind the
/// copy, and whose later ones send one naming the copy's head, then the log's entries from the
/// copy's last command when `repeats`, and hold the connection open to the end of the stint.
async fn rolled_back_then(log: &std::path::Path, repeats: bool) -> Operator {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let connected = Arc::new(AtomicUsize::new(0));
    let (counted, log) = (connected.clone(), log.to_owned());
    let app = Router::new().route(
        "/v1/stream",
        get(move |ws: WebSocketUpgrade, uri: Uri| {
            let (n, log) = (counted.fetch_add(1, Ordering::SeqCst), log.clone());
            async move { ws.on_upgrade(move |socket| held(socket, uri, log, n, repeats)) }
        }),
    );
    let api = dev_config().api;
    let (tls, limits) = (crate::tls::TlsIdentity::generate().unwrap(), Arc::new(ApiLimits::new(&api)));
    tokio::spawn(async move { crate::tls::serve(listener, tls.acceptors, app, limits, api).await });
    Operator { host, connected }
}

/// Serves connection `n` of `rolled_back_then`.
async fn held(mut socket: axum::extract::ws::WebSocket, uri: Uri, log: std::path::PathBuf, n: usize, repeats: bool) {
    let path = uri.path_and_query().map(|query| query.as_str().to_owned()).unwrap_or_default();
    let field = |name: &str| path.split(['?', '&']).find_map(|part| part.strip_prefix(name)).map(str::to_owned);
    let challenge = engine_wire::parse_challenge(&field("challenge=").unwrap()).unwrap();
    let from: u64 = field("from=").unwrap().parse().unwrap();
    let connection = Connection::new(challenge, &path);
    let deployment = Deployment { chain_id: 31_337, vault: [0; 20] };
    let key = EnclaveKey::test_key();
    let all = log_messages(&log, Offset::start_of(0));
    let head = serde_json::from_str::<serde_json::Value>(all.last().unwrap()).unwrap()["offset"].clone();
    let heartbeat = |head: &serde_json::Value| serde_json::json!({"type": "heartbeat", "head": head}).to_string();
    let mut messages = vec![heartbeat(&serde_json::json!("1:0"))];
    if n > 0 {
        messages = vec![heartbeat(&head)];
        if repeats {
            messages.extend(log_messages(&log, Offset::start_of(from)));
        }
    }
    let mut previous = [0u8; 32];
    for (i, msg) in messages.into_iter().enumerate() {
        let time = 1_700_000_000_000u64;
        let digest = engine_wire::frame_digest(&deployment, &connection, &previous, i as u64, time, &msg);
        let sig = key.sign_digest(digest);
        previous = digest;
        let frame = SignedFrame { v: FRAME_VERSION, n: i as u64, time, msg, sig };
        if socket.send(Message::Text(serde_json::to_string(&frame).unwrap().into())).await.is_err() {
            return;
        }
    }
    if n == 0 {
        let _ = socket.send(Message::Close(None)).await;
    } else {
        // Held open until the member's stint ends and drops it.
        while socket.recv().await.is_some() {}
    }
}
