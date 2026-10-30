//! Every local custody refusal precedes history creation/repair and manifest recovery.

use super::*;
use crate::history::HistoryCheck;
use crate::test_support::ANVIL_CONFIG;

#[tokio::test]
async fn custody_refusals_preserve_log_and_all_history_images() {
    for image in ["missing", "corrupt", "wal"] {
        for refusal in ["malformed", "chain", "vault", "ahead", "unreadable"] {
            let root = scratch_dir(&format!("node-cursor-preflight-{image}-{refusal}"));
            let mut cfg = NodeConfig::load(Path::new(ANVIL_CONFIG)).unwrap();
            cfg.node.log_dir = root.join("log");
            cfg.history.dir = root.join("history");
            cfg.alerts.outbox.dir = root.join("outbox");
            cfg.node.listen = "127.0.0.1:0".parse().unwrap();
            cfg.metrics.listen = "127.0.0.1:0".parse().unwrap();
            let cursor = root.join("cursor.json");
            cfg.custody.evm.as_mut().unwrap().cursor_path = cursor.clone();
            fs::create_dir_all(&cfg.node.log_dir).unwrap();
            // A read-only replay accepts this interrupted listing. Opening the writer trims it.
            fs::write(cfg.node.log_dir.join(MANIFEST), br#"{"file":"#).unwrap();
            let mut source_writer = None;
            match image {
                "missing" => {}
                "corrupt" => {
                    fs::create_dir_all(&cfg.history.dir).unwrap();
                    fs::write(cfg.history.dir.join(INDEX_FILE), b"corrupt history").unwrap();
                    fs::write(cfg.history.dir.join(format!("{INDEX_FILE}-wal")), b"recovery evidence").unwrap();
                }
                "wal" => {
                    let source = scratch_dir(&format!("node-cursor-wal-source-{}", rand::random::<u64>()));
                    let writer = HistoryCheck::open(&HistorySection {
                        dir: source.clone(),
                        retention: cfg.history.retention,
                        marks: None,
                    })
                    .unwrap()
                    .finish()
                    .unwrap();
                    fs::create_dir_all(&cfg.history.dir).unwrap();
                    for entry in fs::read_dir(&source).unwrap() {
                        let entry = entry.unwrap();
                        fs::copy(entry.path(), cfg.history.dir.join(entry.file_name())).unwrap();
                    }
                    assert!(fs::metadata(cfg.history.dir.join(format!("{INDEX_FILE}-wal"))).unwrap().len() > 32);
                    source_writer = Some(writer);
                }
                _ => unreachable!(),
            }
            let mut value = serde_json::json!({
                "chain_id": cfg.custody.chain_id, "vault": cfg.custody.vault,
                "next_block": 0, "next_deposit_id": 0, "log_seq": null,
                "last_block_hash": null, "crediting": null
            });
            match refusal {
                "malformed" => fs::write(&cursor, b"{").unwrap(),
                "unreadable" => fs::create_dir(&cursor).unwrap(),
                _ => {
                    match refusal {
                        "chain" => value["chain_id"] = serde_json::json!(1),
                        "vault" => value["vault"] = serde_json::json!("0x0000000000000000000000000000000000000001"),
                        "ahead" => value["log_seq"] = serde_json::json!(0),
                        _ => unreachable!(),
                    }
                    fs::write(&cursor, serde_json::to_vec(&value).unwrap()).unwrap();
                }
            }
            let before = tree(&root);
            let (_guard, mut alerts) = capture_alerts();
            let (telemetry, _layer) = telemetry();
            let result = timeout(Duration::from_secs(10), run(cfg, telemetry)).await.expect("startup terminates");
            assert!(result.is_err(), "{image}/{refusal}");
            let emitted: Vec<_> = iter::from_fn(|| alerts.try_recv().ok()).collect();
            assert!(
                emitted.iter().any(|a| a["alert"] == "custody_connect_failed" && a["level"] == "ERROR"),
                "{image}/{refusal}: {result:?}; {emitted:?}"
            );
            assert_eq!(tree(&root), before, "{image}/{refusal}: refusal changed persisted bytes");
            drop(source_writer);
        }
    }
}

fn tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        // Critical refusal alerts are allowed to persist in their dedicated outbox.
        if path.file_name().is_some_and(|name| name == "outbox") {
            continue;
        }
        if path.is_dir() {
            result.insert(path.clone(), None);
            result.extend(tree(&path));
        } else {
            result.insert(path.clone(), Some(fs::read(path).unwrap()));
        }
    }
    result
}
