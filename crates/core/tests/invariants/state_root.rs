//! The state root under generated sessions. From `V9`, a root computed from what changed since the
//! previous one, at random points in the session, equals the root of the same state hashed from
//! scratch, and every account's proof, and every MONKEY entitlement's, verifies against it. The
//! sessions run under the latest version with emissions initialised, so every rule that changes
//! state reaches the tree, and their days-long gaps finalise emission periods.

use std::cell::Cell;

use engine_core::Engine;
use engine_core::command::{AdminCommand, Command};
use engine_core::eip712::B256;
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::protocol::ProtocolVersion;
use proptest::bool::weighted;
use proptest::collection::vec;
use proptest::test_runner::{TestCaseError, TestRunner};

use crate::access::Clients;
use crate::common::{Harness, operator};
use crate::config;
use crate::session::{command, session};

#[test]
fn incremental_roots_match_a_rebuild_and_every_account_proves_against_them() {
    // Whether to compute a root after each step, so roots fall after one command or after many.
    let roots = vec(weighted(0.1), 200);
    // Whether any session allocated MONKEY, so the entitlement checks do not pass vacuously.
    let allocated = Cell::new(false);
    let result = TestRunner::new(config()).run(&(session(), roots), |(session, roots)| {
        // The FX markets are listed, so sessions also move FX marks and euro and yen positions, under
        // the latest rules, so the tree holds every state they keep.
        let mut genesis = Harness::fx();
        genesis.admin(AdminCommand::SetOperator(operator()));
        let (mut h, clients) = Clients::prepare(genesis, &session);
        assert_eq!(h.engine.protocol_version(), ProtocolVersion::LATEST);
        assert_eq!(h.engine.accounts().len(), 3, "latest-version tree checks need real funded accounts");
        let schedule = EmissionSchedule { start_ms: h.ts, period_budget: TokenUnits(1_000_003) };
        h.operator_admin(AdminCommand::InitEmissions(schedule));
        check(&mut h.engine)?;
        for (step, root) in session.steps.into_iter().zip(roots) {
            h.advance(step.advance_ms);
            let action = step.action.map_accounts(|account| clients.account(account));
            let cmd = clients.sign(command(&action, &h.engine, h.ts), &h.engine, h.ts);
            // From `V8` an operator command applies only signed by the operator.
            match cmd.clone() {
                Command::Admin(admin) if admin.is_operator_command() => h.operate(admin),
                other => h.cmd(other),
            };
            if root {
                check(&mut h.engine).map_err(|e| TestCaseError::fail(format!("after {cmd:?}: {e}")))?;
            }
        }
        allocated.set(allocated.get() || h.engine.emissions().is_some_and(|emissions| emissions.entitled.0 > 0));
        check(&mut h.engine)
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
    assert!(allocated.get(), "no generated session allocated MONKEY");
}

// ── Helpers ──

/// Fails unless the engine's root equals a rebuild's, every account's proof verifies against it
/// with the account's balance, toxicity score, and positions, and every account's entitlement
/// proof verifies with its entitlement; and unless every finalised emission budget is an
/// entitlement or cancelled.
/// Error cases:
/// - the incremental root differs from the root of the state hashed from scratch
/// - an account has no proof, or one that does not verify or does not match the account
/// - an entitlement has no proof, or one that does not verify or does not match it
/// - entitlements and cancelled budgets do not sum to the finalised periods' budgets
fn check(engine: &mut Engine) -> Result<(), TestCaseError> {
    let root = engine.state_root();
    let mut rebuilt: Engine = serde_json::from_slice(&serde_json::to_vec(engine).unwrap()).unwrap();
    if rebuilt.state_root() != root {
        return Err(TestCaseError::fail("the incremental root differs from a rebuild's"));
    }
    for (id, account) in engine.accounts() {
        let proof = engine.account_proof(*id).ok_or_else(|| TestCaseError::fail(format!("no proof for {id}")))?;
        if !proof.verify(B256::from(root))
            || proof.leaf.balance != account.balance
            || proof.leaf.toxicity_score() != account.toxicity_score()
            || !proof.positions.iter().map(|held| (&held.market, &held.position)).eq(&account.positions)
        {
            return Err(TestCaseError::fail(format!("the proof for {id} does not hold its account")));
        }
        let entitled = engine.entitlement(*id);
        let proven = match engine.entitlement_proof(*id) {
            Some(proof) => proof.verify(B256::from(root)) && proof.entitled == entitled,
            None => entitled.0 == 0,
        };
        if !proven {
            return Err(TestCaseError::fail(format!("the entitlement proof for {id} does not hold {entitled}")));
        }
    }
    if let Some(emissions) = engine.emissions() {
        let entitled: u128 = engine.accounts().keys().map(|id| engine.entitlement(*id).0).sum();
        let scheduled = emissions.schedule.budgets(0, emissions.period);
        if entitled != emissions.entitled.0 || emissions.entitled.0 + emissions.cancelled.0 != scheduled {
            return Err(TestCaseError::fail(format!("emissions do not account for every budget: {emissions:?}")));
        }
    }
    Ok(())
}
