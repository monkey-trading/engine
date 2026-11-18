//! Access decisions through signed, sequenced commands and public replay: signed declarations, source
//! evidence, policy refusals, holds, resting orders covered by their placement screening, durable
//! retries of curable refusals with exactly-once credit (`access/pending.rs`), independent wallet
//! vectors for both declarations and source evidence (`fixtures/access-eip712.json`), and the `V24`
//! reference session with its assertions.

mod common;
#[path = "access/declarations.rs"]
mod declarations;
#[path = "access/deposits.rs"]
mod deposits;
#[path = "access/emission_debt.rs"]
mod emission_debt;
#[path = "access/pending.rs"]
mod pending;
#[path = "access/policy.rs"]
mod policy;
#[path = "access/receipts.rs"]
mod receipts;
// The V24 reference-log session, so its assertions and test run with every test pass.
#[path = "../examples/reference_log/v24.rs"]
mod reference_session;
#[path = "access/support.rs"]
mod support;
#[path = "access/vectors.rs"]
mod vectors;
