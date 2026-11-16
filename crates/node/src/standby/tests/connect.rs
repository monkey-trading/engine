//! A stint its `until` ends while it still connects ends there, with nothing appended, rather than at
//! the connection's timeout: an operator that takes the connection and never completes the handshake
//! holds a member waiting to take its turn for its retry pause alone.

use super::*;

use std::time::Instant;

use crate::standby::follow::follow_until;

#[tokio::test]
async fn a_stint_its_until_ends_while_connecting_returns_at_once() {
    let dir = scratch_dir("standby-follow-connecting");
    let (mut writer, mut copied) = open_copy(&dir, 100, Chain::new(DOMAIN, enclave())).unwrap();
    // An operator that accepts each connection and never answers it.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let holding = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    let (ended, until) = tokio::sync::watch::channel(false);
    let (cfg, followed) = (config(), Followed::from_standby(&standby_section(port)));
    let started = Instant::now();
    let budget = (Duration::from_secs(60), Some(until));
    let stint = follow_until(&mut writer, &mut copied, &cfg, &followed, budget, None, None);
    let end = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        ended.send_replace(true);
    };
    let (stinted, ()) = tokio::join!(tokio::time::timeout(Duration::from_secs(10), stint), end);
    let failed = stinted.expect("the stint waited past its end for the handshake").unwrap_err();
    assert!(format!("{failed:#}").contains("before the stream"), "the stint failed as {failed:#}");
    assert!(started.elapsed() < Duration::from_secs(5), "the stint ended at {:?}", started.elapsed());
    holding.abort();
}
