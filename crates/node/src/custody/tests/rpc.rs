//! The RPC adapter's own checks: log decoding, the deposit count's return data, deadlines, and
//! keeping the endpoint URL out of errors.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy::primitives::{Address, B256, Bytes, LogData, U256};
use alloy::rpc::types::Log;
use alloy::sol_types::{SolCall, SolEvent};
use engine_core::types::AccountId;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::config::EvmCustodySection;
use crate::custody::evm::{Beneficiary, BlockRef, VaultReader};
use crate::custody::rpc::{Deposited, HouseDeposited, SurplusBooked, VaultRpc, decode_count, depositCountCall, legacy};
use crate::custody::tests::fakes::{VAULT, evm_toml};

#[test]
fn vault_logs_decode_into_deposits_with_their_position() {
    let rpc = rpc_at("http://127.0.0.1:1");
    let trader = Address::repeat_byte(0xaa);
    let deposit = rpc
        .decode(&log(
            Deposited { id: U256::from(3), account: trader, from: trader, amount: U256::from(5) }.encode_log_data()
        ))
        .unwrap();
    assert_eq!((deposit.block, deposit.block_hash, deposit.log_index, deposit.id), (7, B256::repeat_byte(7), 2, 3));
    assert_eq!((deposit.beneficiary, deposit.units), (Beneficiary::Trader(AccountId(trader.into())), U256::from(5)));
    assert_eq!(deposit.payer, Some(AccountId(trader.into())));
    let house = rpc
        .decode(&log(HouseDeposited { id: U256::from(4), from: trader, amount: U256::from(9) }.encode_log_data()))
        .unwrap();
    assert_eq!((house.beneficiary, house.id), (Beneficiary::House, 4));
    assert_eq!(house.payer, Some(AccountId(trader.into())));
    let surplus =
        rpc.decode(&log(SurplusBooked { id: U256::from(5), amount: U256::from(11) }.encode_log_data())).unwrap();
    assert_eq!((surplus.beneficiary, surplus.id), (Beneficiary::House, 5));
    assert_eq!(surplus.payer, None, "a surplus books no funding address, so screening holds it");
}

#[test]
fn a_named_beneficiary_does_not_replace_the_actual_payer_and_legacy_payers_stay_unknown() {
    let rpc = rpc_at("http://127.0.0.1:1");
    let trader = Address::repeat_byte(0xaa);
    let funder = Address::repeat_byte(0xbb);
    let current = rpc
        .decode(&log(
            Deposited { id: U256::from(3), account: trader, from: funder, amount: U256::from(5) }.encode_log_data()
        ))
        .unwrap();
    assert_eq!(current.beneficiary, Beneficiary::Trader(AccountId(trader.into())));
    assert_eq!(current.payer, Some(AccountId(funder.into())));
    let legacy = rpc
        .decode(&log(legacy::Deposited { id: U256::from(3), account: trader, amount: U256::from(5) }.encode_log_data()))
        .unwrap();
    assert_eq!(legacy.beneficiary, current.beneficiary);
    assert_eq!(legacy.payer, None);
}

#[test]
fn logs_that_are_not_final_vault_deposits_are_refused() {
    let rpc = rpc_at("http://127.0.0.1:1");
    let data = Deposited {
        id: U256::from(3),
        account: Address::repeat_byte(0xaa),
        from: Address::repeat_byte(0xbb),
        amount: U256::from(5),
    }
    .encode_log_data();
    let good = log(data.clone());
    let refused = [
        Log { removed: true, ..good.clone() },
        Log { inner: alloy::primitives::Log { address: Address::repeat_byte(1), data: data.clone() }, ..good.clone() },
        Log { block_number: None, ..good.clone() },
        Log { block_hash: None, ..good.clone() },
        Log { transaction_hash: None, ..good.clone() },
        Log { log_index: None, ..good.clone() },
        log(LogData::new_unchecked(vec![B256::repeat_byte(9)], data.data.clone())),
        log(Deposited {
            id: U256::MAX,
            account: Address::repeat_byte(0xaa),
            from: Address::repeat_byte(0xbb),
            amount: U256::from(5),
        }
        .encode_log_data()),
    ];
    for (i, bad) in refused.iter().enumerate() {
        assert!(rpc.decode(bad).is_err(), "case {i} decoded");
    }
}

#[test]
fn the_deposit_count_decodes_and_reads_no_code_as_zero() {
    assert_eq!(decode_count(&Bytes::new()).unwrap(), 0, "no vault deployed at that block yet");
    assert_eq!(decode_count(&depositCountCall::abi_encode_returns(&U256::from(7)).into()).unwrap(), 7);
    let beyond = depositCountCall::abi_encode_returns(&(U256::from(u64::MAX) + U256::from(1)));
    assert!(decode_count(&beyond.into()).is_err());
}

#[tokio::test]
async fn an_endpoint_that_never_answers_times_out() {
    // Accepts connections into its backlog and never replies.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = rpc_at(&format!("http://{}", listener.local_addr().unwrap()));

    let err = format!("{:#}", rpc.finality().await.unwrap_err());
    assert!(err.contains("timed out after"), "{err}");
}

#[tokio::test]
async fn rpc_errors_never_carry_the_endpoint_url() {
    let rpc = rpc_at("http://127.0.0.1:1/v2/secret-key?token=hunter2");

    let err = format!("{:#}", rpc.finality().await.unwrap_err());
    assert!(err.contains("eth_getBlockByNumber failed"), "{err}");
    assert!(!err.contains("secret-key") && !err.contains("hunter2"), "{err}");
}

#[tokio::test]
async fn a_chain_move_between_the_tag_and_head_reads_is_settled_by_one_re_read() {
    // First pair: the endpoint's tag still names block 8 while a reorg has dropped its head to 7
    // — the tag was read a moment before the head. The re-read then sees a consistent, higher
    // chain; its head and timestamp, not the first pair's, are the answer.
    let (url, safe_calls, latest_calls, server) =
        scripted_chain(&[Some(8), Some(9)], &[Some(7), Some(9)], usize::MAX).await;
    let rpc = rpc_at(&url);

    let finality = rpc.finality().await.unwrap();
    assert_eq!(finality.head, 9);
    assert_eq!(finality.head_time, 1009);
    assert_eq!(finality.tagged, Some(BlockRef { number: 9, hash: B256::repeat_byte(9) }));
    assert_eq!(safe_calls.load(Ordering::Relaxed), 2, "the inconsistent pair is re-read once");
    assert_eq!(latest_calls.load(Ordering::Relaxed), 2);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn a_pair_that_stays_inconsistent_is_returned_after_one_re_read() {
    // The quorum's contradiction check, not an endless retry, classifies a provider whose tag
    // keeps sitting above its head; the second pair is returned as read.
    let (url, safe_calls, latest_calls, server) =
        scripted_chain(&[Some(8), Some(9)], &[Some(7), Some(8)], usize::MAX).await;
    let rpc = rpc_at(&url);

    let finality = rpc.finality().await.unwrap();
    assert_eq!(finality.head, 8);
    assert_eq!(finality.head_time, 1008);
    assert_eq!(finality.tagged, Some(BlockRef { number: 9, hash: B256::repeat_byte(9) }));
    assert_eq!(safe_calls.load(Ordering::Relaxed), 2, "the re-read is bounded at one");
    assert_eq!(latest_calls.load(Ordering::Relaxed), 2);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn a_tag_at_the_head_or_missing_needs_no_re_read() {
    // The boundary the consistency check must hold: a tag at the head (a `<=` a `<` would
    // re-read every `latest`-tagged poll forever) and no block at the tag yet.
    for (safe, expected) in [(Some(7), Some(BlockRef { number: 7, hash: B256::repeat_byte(7) })), (None, None)] {
        let (url, safe_calls, latest_calls, server) = scripted_chain(&[safe], &[Some(7)], usize::MAX).await;
        let rpc = rpc_at(&url);

        let finality = rpc.finality().await.unwrap();
        assert_eq!(finality.head, 7);
        assert_eq!(finality.tagged, expected);
        assert_eq!(safe_calls.load(Ordering::Relaxed), 1, "a consistent pair is not re-read");
        assert_eq!(latest_calls.load(Ordering::Relaxed), 1);
        server.abort();
        let _ = server.await;
    }
}

#[tokio::test]
async fn a_null_tag_on_the_re_read_is_a_consistent_answer() {
    // The re-read's tag can also come back empty while the chain has no final block yet.
    let (url, safe_calls, latest_calls, server) = scripted_chain(&[Some(8), None], &[Some(7)], usize::MAX).await;
    let rpc = rpc_at(&url);

    let finality = rpc.finality().await.unwrap();
    assert_eq!(finality.head, 7);
    assert_eq!(finality.tagged, None);
    assert_eq!(safe_calls.load(Ordering::Relaxed), 2);
    assert_eq!(latest_calls.load(Ordering::Relaxed), 2);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn a_failed_re_read_propagates_and_stays_bounded() {
    // The re-read's own transport failure surfaces as the read's error after the one retry.
    let (url, safe_calls, latest_calls, server) = scripted_chain(&[Some(8), Some(8)], &[Some(7)], 2).await;
    let rpc = rpc_at(&url);

    let err = format!("{:#}", rpc.finality().await.unwrap_err());
    assert!(err.contains("eth_getBlockByNumber failed"), "{err}");
    assert_eq!(safe_calls.load(Ordering::Relaxed), 2);
    assert_eq!(latest_calls.load(Ordering::Relaxed), 1, "the failing re-read is not retried again");
    server.abort();
    let _ = server.await;
}

// ── Helpers ──

/// A loopback endpoint whose `eth_getBlockByNumber` answers `safe` and `latest` from their
/// scripts, repeating each script's last entry once it runs out; a `None` entry is a null block,
/// and requests after the `fail_after`th return a JSON-RPC error. Counts requests per tag.
/// Block `n` carries hash `n` and timestamp `1000 + n`.
async fn scripted_chain(
    safe: &[Option<u64>],
    latest: &[Option<u64>],
    fail_after: usize,
) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>, JoinHandle<()>) {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use alloy::rpc::types::Block;
    use axum::extract::State;
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};

    #[derive(Clone)]
    struct Script(Arc<Mutex<VecDeque<Option<u64>>>>);

    impl Script {
        fn next(&self) -> Option<u64> {
            let mut blocks = self.0.lock().unwrap();
            let block = *blocks.front().expect("a scripted chain needs at least one block");
            if blocks.len() > 1 {
                blocks.pop_front();
            }
            block
        }
    }

    #[derive(Clone)]
    struct ChainServer {
        safe: Script,
        latest: Script,
        safe_calls: Arc<AtomicUsize>,
        latest_calls: Arc<AtomicUsize>,
        fail_after: usize,
    }

    async fn answer(State(server): State<ChainServer>, Json(request): Json<Value>) -> Json<Value> {
        let (script, counter) = match request["params"][0].as_str().unwrap() {
            "safe" => (&server.safe, &server.safe_calls),
            "latest" => (&server.latest, &server.latest_calls),
            other => panic!("unexpected block tag {other}"),
        };
        counter.fetch_add(1, Ordering::Relaxed);
        let count = server.safe_calls.load(Ordering::Relaxed) + server.latest_calls.load(Ordering::Relaxed);
        if count > server.fail_after {
            return Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"boom"}}));
        }
        let number = script.next();
        let result = number.map(|number| {
            let mut block = Block::<alloy::rpc::types::Transaction>::default();
            block.header.number = number;
            block.header.timestamp = 1000 + number;
            block.header.hash = B256::repeat_byte(number as u8);
            serde_json::to_value(block).unwrap()
        });
        Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let state = ChainServer {
        safe: Script(Arc::new(Mutex::new(safe.iter().copied().collect()))),
        latest: Script(Arc::new(Mutex::new(latest.iter().copied().collect()))),
        safe_calls: Arc::new(AtomicUsize::new(0)),
        latest_calls: Arc::new(AtomicUsize::new(0)),
        fail_after,
    };
    let (safe_calls, latest_calls) = (state.safe_calls.clone(), state.latest_calls.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/", post(answer)).with_state(state)).await.unwrap();
    });
    (url, safe_calls, latest_calls, server)
}

/// A client for `url` with a short deadline.
fn rpc_at(url: &str) -> VaultRpc {
    let toml = evm_toml(&std::env::temp_dir(), 1_000)
        .replace("rpc_url = \"http://127.0.0.1:1\"", &format!("rpc_url = \"{url}\""))
        .replace("rpc_timeout = \"1s\"", "rpc_timeout = \"200ms\"");
    let cfg: EvmCustodySection = toml::from_str(&toml).unwrap();
    VaultRpc::new(&cfg, &cfg.providers[0].rpc_url, VAULT).unwrap()
}

/// A vault log in block 7 at log index 2.
fn log(data: LogData) -> Log {
    Log {
        inner: alloy::primitives::Log { address: VAULT, data },
        block_hash: Some(B256::repeat_byte(7)),
        block_number: Some(7),
        transaction_hash: Some(B256::repeat_byte(1)),
        log_index: Some(2),
        ..Log::default()
    }
}

#[test]
fn null_and_wrong_height_headers_are_typed_content_errors_and_valid_headers_keep_their_values() {
    use crate::custody::rpc::{InvalidRpcResponse, decode_header};
    let mut header: alloy::rpc::types::Header = Default::default();
    header.inner.number = 7;
    header.hash = B256::repeat_byte(7);
    header.inner.parent_hash = B256::repeat_byte(6);
    let decoded = decode_header(Some(header.clone()), 7).unwrap();
    assert_eq!((decoded.number, decoded.hash, decoded.parent_hash), (7, header.hash, header.parent_hash));
    for result in [None, Some(header)] {
        let err = decode_header(result, 8).unwrap_err();
        assert!(err.is::<InvalidRpcResponse>(), "{err:#}");
        assert!(format!("{err:#}").contains('8'));
    }
}

#[tokio::test]
async fn malformed_successful_header_payloads_are_content_errors_but_timeouts_are_not() {
    use crate::custody::rpc::InvalidRpcResponse;
    use axum::{Json, Router, routing::post};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/",
        post(|Json(request): Json<serde_json::Value>| async move {
            let replies: Vec<_> = request.as_array().unwrap().iter().map(|request| {
            serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":{"number":"not-a-number"}})
        }).collect();
            Json(replies)
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let err = rpc_at(&url).headers(1, 1).await.unwrap_err();
    server.abort();
    let _ = server.await;
    assert!(err.is::<InvalidRpcResponse>(), "{err:#}");
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err = rpc_at(&format!("http://{}", silent.local_addr().unwrap())).headers(1, 1).await.unwrap_err();
    assert!(!err.is::<InvalidRpcResponse>(), "{err:#}");
    assert!(format!("{err:#}").contains("timed out"));
}

#[tokio::test]
async fn rpc_wire_keeps_tags_batches_filters_quantities_and_canonical_hash_calls() {
    use alloy::rpc::types::Block;
    use axum::extract::State;
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    use tokio::sync::mpsc;

    use crate::custody::rpc::{InvalidRpcResponse, usdcCall};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, mut requests) = mpsc::unbounded_channel();
    let app = Router::new().route("/", post(wire_answer)).with_state(tx);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let rpc = rpc_at(&url);
    let identity = rpc.identity().await.unwrap();
    assert_eq!(identity.chain_id, 31337);
    assert_eq!(identity.token, Some(Address::repeat_byte(2)));
    let finality = rpc.finality().await.unwrap();
    assert_eq!((finality.head, finality.head_time), (12, 1234));
    assert_eq!(finality.tagged, Some(BlockRef { number: 7, hash: B256::repeat_byte(7) }));
    let headers = rpc.headers(6, 7).await.unwrap();
    assert_eq!(
        headers.iter().map(|h| (h.number, h.hash)).collect::<Vec<_>>(),
        vec![(6, B256::repeat_byte(6)), (7, B256::repeat_byte(7))]
    );
    for number in [8, 9] {
        let error = rpc.headers(number, number).await.unwrap_err();
        assert!(error.is::<InvalidRpcResponse>(), "{error:#}");
    }
    assert!(rpc.deposits(6, 7).await.unwrap().is_empty());
    assert_eq!(rpc.deposit_count(BlockRef { number: 7, hash: B256::repeat_byte(7) }).await.unwrap(), 4);
    server.abort();
    let _ = server.await;
    let mut observed = Vec::new();
    while let Ok(request) = requests.try_recv() {
        observed.push(request);
    }
    let call = |input: Vec<u8>| json!({"to": VAULT, "input": Bytes::from(input)});
    let compact = |method: &str, params: Value| json!({"method":method,"params":params});
    let expected = vec![
        compact("eth_chainId", json!([])),
        compact("eth_call", json!([call(usdcCall {}.abi_encode()), "latest"])),
        compact("eth_getBlockByNumber", json!(["safe", false])),
        compact("eth_getBlockByNumber", json!(["latest", false])),
        json!([
            compact("eth_getBlockByNumber", json!(["0x6", false])),
            compact("eth_getBlockByNumber", json!(["0x7", false]))
        ]),
        json!([compact("eth_getBlockByNumber", json!(["0x8", false]))]),
        json!([compact("eth_getBlockByNumber", json!(["0x9", false]))]),
        compact(
            "eth_getLogs",
            json!([{"address":VAULT,"fromBlock":"0x6","toBlock":"0x7","topics":[[Deposited::SIGNATURE_HASH,HouseDeposited::SIGNATURE_HASH,SurplusBooked::SIGNATURE_HASH,legacy::Deposited::SIGNATURE_HASH]]}]),
        ),
        compact(
            "eth_call",
            json!([call(depositCountCall {}.abi_encode()), {"blockHash":B256::repeat_byte(7),"requireCanonical":true}]),
        ),
    ];
    let canonicalize = |mut requests: Vec<Value>| {
        // Only alternatives within one topic position are unordered.
        let alternatives = requests[7]["params"][0]["topics"][0].as_array_mut().unwrap();
        alternatives.sort_unstable_by(|left, right| left.as_str().unwrap().cmp(right.as_str().unwrap()));
        requests
    };
    let expected = canonicalize(expected);
    assert_eq!(canonicalize(observed.clone()), expected);
    for (alternatives, matches) in [
        (
            json!([
                Deposited::SIGNATURE_HASH,
                HouseDeposited::SIGNATURE_HASH,
                SurplusBooked::SIGNATURE_HASH,
                legacy::Deposited::SIGNATURE_HASH
            ]),
            true,
        ),
        (
            json!([
                legacy::Deposited::SIGNATURE_HASH,
                SurplusBooked::SIGNATURE_HASH,
                HouseDeposited::SIGNATURE_HASH,
                Deposited::SIGNATURE_HASH
            ]),
            true,
        ),
        (json!([Deposited::SIGNATURE_HASH, HouseDeposited::SIGNATURE_HASH]), false),
        (json!([Deposited::SIGNATURE_HASH]), false),
        (json!([Deposited::SIGNATURE_HASH, Deposited::SIGNATURE_HASH]), false),
        (json!([Deposited::SIGNATURE_HASH, HouseDeposited::SIGNATURE_HASH, Deposited::SIGNATURE_HASH]), false),
        (json!([Deposited::SIGNATURE_HASH, B256::repeat_byte(9)]), false),
    ] {
        let mut variant = observed.clone();
        variant[7]["params"][0]["topics"][0] = alternatives;
        assert_eq!(canonicalize(variant) == expected, matches);
    }

    async fn wire_answer(State(tx): State<mpsc::UnboundedSender<Value>>, Json(request): Json<Value>) -> Json<Value> {
        fn answer(request: &Value) -> Value {
            let result = match request["method"].as_str().unwrap() {
                "eth_chainId" => json!("0x7a69"),
                "eth_call" if request["params"][1] == "latest" => {
                    json!(Bytes::from(usdcCall::abi_encode_returns(&Address::repeat_byte(2))))
                }
                "eth_call" => json!(Bytes::from(depositCountCall::abi_encode_returns(&U256::from(4)))),
                "eth_getLogs" => json!([]),
                "eth_getBlockByNumber" => {
                    let number = match request["params"][0].as_str().unwrap() {
                        "safe" => 7,
                        "latest" => 12,
                        value => u64::from_str_radix(value.trim_start_matches("0x"), 16).unwrap(),
                    };
                    if number == 8 {
                        Value::Null
                    } else {
                        let mut block = Block::<alloy::rpc::types::Transaction>::default();
                        block.header.number = if number == 9 { 10 } else { number };
                        block.header.timestamp = 1234;
                        block.header.hash = B256::repeat_byte(number as u8);
                        serde_json::to_value(block).unwrap()
                    }
                }
                method => panic!("unexpected method {method}"),
            };
            json!({"jsonrpc":"2.0","id":request["id"],"result":result})
        }
        fn compact(request: &Value) -> Value {
            json!({"method":request["method"],"params":request.get("params").cloned().unwrap_or(json!([]))})
        }
        let result = if let Some(batch) = request.as_array() {
            tx.send(Value::Array(batch.iter().map(compact).collect())).unwrap();
            Value::Array(batch.iter().map(answer).collect())
        } else {
            tx.send(compact(&request)).unwrap();
            answer(&request)
        };
        Json(result)
    }
}

mod staking;
