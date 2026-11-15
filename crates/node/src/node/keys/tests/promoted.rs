//! A promoted node's release watch, which starts once its promotion is final.

use super::*;
use crate::checkpoints::CurrentRelease;
use vault::Vault;

/// Until the vault's finalized view names a promoted node's key, the finalised schedule still names
/// the outgoing holding: the watch pages nothing then, and judges the release once the promotion is
/// final.
#[tokio::test(start_paused = true)]
async fn a_promoted_nodes_release_watch_waits_for_its_promotion_to_be_final() {
    let (_guard, mut alerts) = capture_alerts();
    let sealed = Release { launch_digest: DIGEST, config_sha256: [0xc1; 32] };
    let vault = Vault::naming(CurrentRelease { id: [0x99; 32], generation: 3 });
    let (signer, promoted) = (vault.signer.clone(), AccountId([0x5e; 20]));
    let (store, every) = (sealed_store("keys-watch-promoted", sealed, 2), Duration::from_secs(60));
    let watch = crate::node::liveness::watch_release(vault, (store, Some(promoted)), every, untrusted());
    let watch = tokio::spawn(watch);
    tokio::time::sleep(every * 3).await;
    assert!(alerts.try_recv().is_err(), "the watch paged before the promotion was final");
    *signer.lock().unwrap() = promoted;
    tokio::time::sleep(every * 3).await;
    let alert = alerts.try_recv().expect("once final, a release the vault does not name pages");
    assert_eq!(alert["alert"].as_str(), "release_superseded");
    watch.abort();
}
