//! Startup refuses a log whose emission period budget differs from the configured minter's MONKEY cap,
//! and a minter whose cap cannot be read, with a Critical alert for each.

use std::time::Duration;

use alloy::primitives::Address;
use anyhow::anyhow;
use engine_core::emissions::{EmissionSchedule, Emissions, TokenUnits};
use engine_core::types::AccountId;

use crate::checkpoints::{CheckpointChain, Poster, VaultState, View};
use crate::config::NodeConfig;
use crate::log::{Checkpoint, CheckpointDomain};
use crate::node::preflight::emission_preflight;
use crate::test_support::{ANVIL_CONFIG, capture_alerts, dev_config};

/// A chain on which `minter`'s MONKEY cap gives the budget, or cannot be read; the preflight must ask
/// for that minter's.
struct CapChain {
    minter: Address,
    budget: Option<TokenUnits>,
}

impl CheckpointChain for CapChain {
    async fn state(&self, _: View) -> anyhow::Result<VaultState> {
        unreachable!("the emission preflight reads only the cap")
    }

    async fn post(&self, _: &Checkpoint) -> anyhow::Result<()> {
        unreachable!("the emission preflight posts nothing")
    }

    async fn emission_budget(&self, minter: Address) -> anyhow::Result<TokenUnits> {
        assert_eq!(minter, self.minter, "the preflight reads the configured minter's cap");
        self.budget.ok_or_else(|| anyhow!("the minter has no MONKEY token"))
    }
}

#[tokio::test]
async fn startup_refuses_another_budget_than_the_minters_and_an_unreadable_cap() {
    let (_guard, mut alerts) = capture_alerts();
    let cfg = evm_config();
    let recorded = |budget| Emissions {
        schedule: EmissionSchedule { start_ms: 0, period_budget: TokenUnits(budget) },
        period: 3,
        entitled: TokenUnits(0),
        cancelled: TokenUnits(0),
    };
    // A log with no schedule yet, or the minter's, starts, and the API learns the minter's budget.
    for emissions in [None, Some(recorded(7))] {
        let budget = emission_preflight(&cfg, &poster(&cfg, Some(TokenUnits(7))), emissions.as_ref()).await;
        assert_eq!(budget.unwrap(), TokenUnits(7));
    }
    assert!(alerts.try_recv().is_err(), "a matching budget raises nothing");

    let refused = emission_preflight(&cfg, &poster(&cfg, Some(TokenUnits(7))), Some(&recorded(8))).await.unwrap_err();
    assert!(refused.to_string().contains("8 differs from the minter's 7"), "{refused:#}");
    let alert = alerts.try_recv().expect("a mismatch raises Critical");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("emission_budget_mismatch", "ERROR"));

    assert!(emission_preflight(&cfg, &poster(&cfg, None), None).await.is_err());
    let alert = alerts.try_recv().expect("an unreadable cap raises Critical");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("checkpoint_startup_failed", "ERROR"));
}

#[tokio::test]
async fn startup_refuses_checkpoints_without_a_minter_to_read() {
    let (_guard, mut alerts) = capture_alerts();
    let mock = dev_config();
    assert!(mock.custody.evm.is_none());
    let refused = emission_preflight(&mock, &poster(&evm_config(), Some(TokenUnits(7))), None).await.unwrap_err();
    assert!(refused.to_string().contains("not EVM"), "{refused:#}");
    let alert = alerts.try_recv().expect("no minter raises Critical");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("checkpoint_startup_failed", "ERROR"));
}

// ── Helpers ──

/// `config/anvil.toml`, whose EVM custody names a minter.
fn evm_config() -> NodeConfig {
    toml::from_str(&std::fs::read_to_string(ANVIL_CONFIG).unwrap()).unwrap()
}

/// A checkpoint worker over a chain on which `cfg`'s minter's cap gives `budget`, or none.
fn poster(cfg: &NodeConfig, budget: Option<TokenUnits>) -> Poster<CapChain> {
    let minter = cfg.custody.evm.as_ref().unwrap().minter;
    Poster {
        chain: CapChain { minter, budget },
        registry: None,
        domain: CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) },
        signer: AccountId([1; 20]),
        retry: Duration::from_millis(1),
        pending_deposits_critical_after: Duration::from_secs(1),
        post_withheld: Default::default(),
        episodes: Default::default(),
        relayer_gas: None,
    }
}
