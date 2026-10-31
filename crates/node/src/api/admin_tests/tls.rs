//! `engine admin` against a live node over TLS: the IPv6 literal target and the pinned-key
//! refusal.

use super::*;

#[tokio::test]
async fn engine_admin_reaches_a_node_on_an_ipv6_literal() {
    let node = Node::start("cli-ipv6").await;
    let url = format!("https://{}", node.serve_on("[::1]:0").await);
    assert!(url.starts_with("https://[::1]:"), "{url}");
    let key = KeyEnv { key_file: None, key: Some(HexKey(format!("0x{}", hex::encode(DEV_KEY)))) };
    admin::run_with_clock(node.args(&url, &["set-reduce-only", "true", "--market", "1"]), key, || NOW).await.unwrap();
    assert!(replay(&node.entries()).unwrap().engine.markets()[&1].reduce_only);
}

#[tokio::test]
async fn engine_admin_refuses_a_node_whose_tls_key_is_not_the_pinned_one() {
    let node = Node::start("cli-pin").await;
    let url = format!("https://{}", node.serve().await);
    let key = KeyEnv { key_file: None, key: Some(HexKey(format!("0x{}", hex::encode(DEV_KEY)))) };
    let other = hex::encode(TlsIdentity::generate().unwrap().public_key_sha256);
    let argv = ["engine", "--config", DEV_CONFIG, "--node", &url, "--pin", &other, "set-reduce-only", "true"];
    let err = admin::run_with_clock(Cli::try_parse_from(argv).unwrap().args, key, || NOW).await.unwrap_err();
    assert!(format!("{err:#}").contains("not the pinned one"), "{err:#}");
    assert!(admin_entries(&node.entries()).is_empty());
}
