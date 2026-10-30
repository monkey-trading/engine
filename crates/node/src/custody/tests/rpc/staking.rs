//! Hash-pinned log reads must fail closed when a log backend lacks the proven fork.
use super::*;
use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};

#[tokio::test]
async fn staking_logs_require_every_proven_hash_even_when_headers_are_canonical() {
    for (missing, partial) in [(false, false), (false, true), (true, false), (true, true)] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(AtomicUsize::new(0));
        let state = (missing, partial, seen.clone());
        let app = Router::new().route("/", post(answer)).with_state(state);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let toml = evm_toml(&std::env::temp_dir(), 1_000)
            .replace("rpc_url = \"http://127.0.0.1:1\"", &format!("rpc_url = \"{url}\""));
        let mut cfg: EvmCustodySection = toml::from_str(&toml).unwrap();
        cfg.staking = Some(
            toml::from_str(&format!(
                "contract = \"{VAULT}\"\ncursor_path = \"unused-staking-cursor\"\n[registry]\ncontract = \"{}\"",
                Address::repeat_byte(0x6b)
            ))
            .unwrap(),
        );
        let rpc = VaultRpc::new(&cfg, &cfg.providers[0].rpc_url, VAULT).unwrap();
        let blocks =
            [BlockRef { number: 6, hash: B256::repeat_byte(6) }, BlockRef { number: 7, hash: B256::repeat_byte(7) }];
        let headers = rpc.headers(6, 7).await.unwrap();
        assert_eq!(
            headers.iter().map(|header| header.hash).collect::<Vec<_>>(),
            blocks.iter().map(|block| block.hash).collect::<Vec<_>>()
        );
        let result = rpc.staking_events(&blocks).await;
        assert_eq!(result.is_err(), missing);
        assert_eq!(seen.load(Ordering::Relaxed), 2, "every proven block is explicitly requested");
        if !missing {
            let events = result.unwrap();
            assert_eq!(events.len(), if partial { 2 } else { 0 });
            assert!(events.windows(2).all(|pair| pair[0].log_index < pair[1].log_index));
        }
        server.abort();
    }
}

async fn answer(
    State((missing, partial, seen)): State<(bool, bool, Arc<AtomicUsize>)>,
    Json(request): Json<Value>,
) -> Json<Value> {
    fn response(request: &Value, missing: bool, partial: bool, seen: &AtomicUsize) -> Value {
        let id = request["id"].clone();
        if request["method"] == "eth_getBlockByNumber" {
            let number =
                u64::from_str_radix(request["params"][0].as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
            let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
            block.header.number = number;
            block.header.hash = B256::repeat_byte(number as u8);
            return json!({"jsonrpc":"2.0","id":id,"result":block});
        }
        assert_eq!(request["method"], "eth_getLogs");
        seen.fetch_add(1, Ordering::Relaxed);
        let filter = &request["params"][0];
        let hash = filter["blockHash"].as_str();
        // Height-only log requests silently return the shared prefix, despite canonical headers.
        if hash.is_some() {
            assert!(filter.get("fromBlock").is_none() && filter.get("toBlock").is_none());
            if missing && hash == Some(format!("{}", B256::repeat_byte(7)).as_str()) {
                return json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"unknown block"}});
            }
        }
        let logs = if partial && hash != Some(format!("{}", B256::repeat_byte(7)).as_str()) {
            [2, 1]
                .into_iter()
                .map(|index| {
                    let mut event = log(crate::custody::rpc::Staked {
                        account: Address::repeat_byte(1),
                        amount: U256::from(index + 1),
                    }
                    .encode_log_data());
                    event.block_number = Some(6);
                    event.block_hash = Some(B256::repeat_byte(6));
                    event.log_index = Some(index);
                    event
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        json!({"jsonrpc":"2.0","id":id,"result":logs})
    }
    Json(if let Some(batch) = request.as_array() {
        Value::Array(batch.iter().map(|request| response(request, missing, partial, &seen)).collect())
    } else {
        response(&request, missing, partial, &seen)
    })
}
