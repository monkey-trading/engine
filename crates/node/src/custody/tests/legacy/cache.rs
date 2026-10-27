//! Full canonical legacy proofs retain obligations and sparse anchors, not consumed headers.

use super::*;
use crate::custody::evm::{VaultIdentity, VaultReader};

#[tokio::test]
async fn many_empty_legacy_windows_keep_bounded_headers_and_the_same_authorization() {
    for (range, height) in [(1, 31), (4, 255), (7, 511)] {
        let dir = scratch("legacy-bounded-headers");
        let cfg: EvmCustodySection = toml::from_str(&evm_toml(&dir, range)).unwrap();
        let (engine, ledger, approvals, receipt) = history();
        let before = serde_json::to_value(&engine).unwrap();
        let chains = long_chains(height, Some(receipt));
        let mut scan = DepositWatcher::start_unscreened(quorum(&chains), &cfg, CHAIN_ID, VAULT, None).unwrap();
        let authorized = scan.prove_legacy_approvals(&engine, &ledger, &approvals).await.unwrap();
        assert_eq!(authorized, BTreeMap::from([((TRADER, 1), 2)]));
        assert_eq!(serde_json::to_value(&engine).unwrap(), before, "proof cannot apply cash or release a lock");
        assert!(
            scan.chain.window_peak() <= 2 * range as usize,
            "processed headers accumulated: range={range}, height={height}, peak={}",
            scan.chain.window_peak()
        );
        let expected_anchors = (height + 1).div_ceil(range) as usize;
        assert_eq!(scan.chain.len(), expected_anchors, "window eviction must preserve the sparse proof anchors");
        let reader = quorum(&chains);
        assert_eq!(reader.identity().await.unwrap(), VaultIdentity { chain_id: CHAIN_ID, token: Some(USDC_TOKEN) });
        let tagged = reader.withdrawal_finality().await.unwrap().tagged.unwrap();
        assert_eq!(scan.chain.get(height), Some(tagged.hash));
    }
}

#[tokio::test]
async fn bounded_legacy_header_retention_does_not_weaken_receipt_or_quorum_refusals() {
    let dir = scratch("legacy-bounded-refusals");
    let cfg: EvmCustodySection = toml::from_str(&evm_toml(&dir, 4)).unwrap();
    let (engine, ledger, approvals, receipt) = history();
    let before = serde_json::to_value(&engine).unwrap();
    for changed in [None, Some(VaultDeposit { units: U256::from(101 * USDC), ..receipt.clone() })] {
        let chains = long_chains(255, changed);
        let mut scan = DepositWatcher::start_unscreened(quorum(&chains), &cfg, CHAIN_ID, VAULT, None).unwrap();
        let error = scan.prove_legacy_approvals(&engine, &ledger, &approvals).await.unwrap_err();
        assert!(
            error.to_string().contains("canonical finality proof")
                || error.to_string().contains("changed beneficiary or amount"),
            "{error:#}"
        );
        assert!(scan.chain.window_peak() <= 8);
        assert_eq!(serde_json::to_value(&engine).unwrap(), before);
    }
    let chains = long_chains(255, Some(receipt));
    for chain in &chains[..2] {
        chain.0.lock().unwrap().failing = true;
    }
    let mut scan = DepositWatcher::start_unscreened(quorum(&chains), &cfg, CHAIN_ID, VAULT, None).unwrap();
    let error = scan.prove_legacy_approvals(&engine, &ledger, &approvals).await.unwrap_err();
    assert!(error.to_string().contains("quorum"), "{error:#}");
    assert_eq!(serde_json::to_value(&engine).unwrap(), before);
}

/// Uses the existing three-provider fake with a long, explicitly controlled final chain.
fn long_chains(height: u64, receipt: Option<VaultDeposit>) -> [FakeChain; 3] {
    let chains = chains(Some(height), receipt);
    for chain in &chains {
        let mut state = chain.0.lock().unwrap();
        state.head = height;
        state.final_block = Some(height);
    }
    chains
}
