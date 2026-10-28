//! Hash-pinned log reads must fail closed when a log backend lacks the proven fork.
use super::*;
use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};

#[tokio::test]
async fn staking_logs_require_every_proven_hash_even_when_headers_are_canonical() {
    for missing in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(AtomicUsize::new(0));
        let state = (missing, seen.clone());
        let app = Router::new().route("/", post(answer)).with_state(state);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let toml = evm_toml(&std::env::temp_dir(), 1_000)
            .replace("rpc_url = \"http://127.0.0.1:1\"", &format!("rpc_url = \"{url}\""));
        let mut cfg: EvmCustodySection = toml::from_str(&toml).unwrap();
        cfg.staking = Some(crate::config::StakingCustodySection {
            contract: VAULT,
            cursor_path: std::env::temp_dir().join("unused-staking-cursor"),
        });
        let rpc = VaultRpc::new(&cfg, &cfg.providers[0].rpc_url, VAULT).unwrap();
        let blocks =
            [BlockRef { number: 6, hash: B256::repeat_byte(6) }, BlockRef { number: 7, hash: B256::repeat_byte(7) }];
        let result = rpc.staking_events(&blocks).await;
        assert_eq!(result.is_err(), missing);
        assert_eq!(seen.load(Ordering::Relaxed), 2, "every proven block is explicitly requested");
        if !missing {
            assert!(result.unwrap().is_empty());
        }
        server.abort();
    }
}

async fn answer(State((missing, seen)): State<(bool, Arc<AtomicUsize>)>, Json(request): Json<Value>) -> Json<Value> {
    let id = request["id"].clone();
    // A height-only backend would silently omit fork-specific events, despite canonical headers.
    if request["method"] == "eth_getLogs" {
        seen.fetch_add(1, Ordering::Relaxed);
        let filter = &request["params"][0];
        let hash = filter["blockHash"].as_str();
        if hash.is_none() {
            return Json(json!({"jsonrpc":"2.0","id":id,"result":[]}));
        }
        assert!(filter.get("fromBlock").is_none() && filter.get("toBlock").is_none());
        if missing && hash == Some(format!("{}", B256::repeat_byte(7)).as_str()) {
            return Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"unknown block"}}));
        }
        return Json(json!({"jsonrpc":"2.0","id":id,"result":[]}));
    }
    panic!("unexpected RPC request: {request}");
}
