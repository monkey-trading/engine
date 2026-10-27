//! Access decisions through signed, sequenced commands and public replay.

mod common;
#[path = "access/declarations.rs"]
mod declarations;
#[path = "access/deposits.rs"]
mod deposits;
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
