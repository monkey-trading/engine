//! Startup refuses a log whose emission period budget differs from the vault's MONKEY cap, and a
//! vault whose cap cannot be read, with a Critical alert for each.

use std::time::Duration;

use anyhow::anyhow;
use engine_core::emissions::{EmissionSchedule, Emissions, TokenUnits};
use engine_core::types::AccountId;

use crate::checkpoints::{CheckpointChain, Poster, VaultState, View};
use crate::log::{Checkpoint, CheckpointDomain};
use crate::node::preflight::emission_preflight;
use crate::test_support::capture_alerts;

/// A vault whose MONKEY cap gives the budget, or cannot be read.
struct CapChain(Option<TokenUnits>);

impl CheckpointChain for CapChain {
    async fn state(&self, _: View) -> anyhow::Result<VaultState> {
        unreachable!("the emission preflight reads only the cap")
    }

    async fn post(&self, _: &Checkpoint) -> anyhow::Result<()> {
        unreachable!("the emission preflight posts nothing")
    }

    async fn emission_budget(&self) -> anyhow::Result<TokenUnits> {
        self.0.ok_or_else(|| anyhow!("the vault has no MONKEY token"))
    }
}

#[tokio::test]
async fn startup_refuses_another_budget_than_the_vaults_and_an_unreadable_cap() {
    let (_guard, mut alerts) = capture_alerts();
    let recorded = |budget| Emissions {
        schedule: EmissionSchedule { start_ms: 0, period_budget: TokenUnits(budget) },
        period: 3,
        entitled: TokenUnits(0),
        cancelled: TokenUnits(0),
    };
    // A log with no schedule yet, or the vault's, starts, and the API learns the vault's budget.
    for emissions in [None, Some(recorded(7))] {
        assert_eq!(emission_preflight(&poster(Some(TokenUnits(7))), emissions.as_ref()).await.unwrap(), TokenUnits(7));
    }
    assert!(alerts.try_recv().is_err(), "a matching budget raises nothing");

    let refused = emission_preflight(&poster(Some(TokenUnits(7))), Some(&recorded(8))).await.unwrap_err();
    assert!(refused.to_string().contains("8 differs from the vault's 7"), "{refused:#}");
    let alert = alerts.try_recv().expect("a mismatch raises Critical");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("emission_budget_mismatch", "ERROR"));

    assert!(emission_preflight(&poster(None), None).await.is_err());
    let alert = alerts.try_recv().expect("an unreadable cap raises Critical");
    assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("checkpoint_startup_failed", "ERROR"));
}

// ── Helpers ──

/// A checkpoint worker over a vault whose cap gives `budget`, or none.
fn poster(budget: Option<TokenUnits>) -> Poster<CapChain> {
    Poster {
        chain: CapChain(budget),
        registry: None,
        domain: CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) },
        signer: AccountId([1; 20]),
        retry: Duration::from_millis(1),
        pending_deposits_critical_after: Duration::from_secs(1),
        post_withheld: Default::default(),
    }
}
