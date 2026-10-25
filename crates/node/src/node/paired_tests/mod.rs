//! Paired recovery through the node's startup order: preflight, activation, and restart.

mod anchor;
mod custody;
pub(crate) mod harness;
mod http;
mod publication;
mod restart;
mod scenario;
mod startup;
mod tail;
