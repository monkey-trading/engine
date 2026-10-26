//! Corporate actions exercise the public signed command path and Engine::apply boundaries.

#[path = "corporate_actions/cash.rs"]
mod cash;
mod common;
#[path = "corporate_actions/rounding.rs"]
mod rounding;
#[path = "corporate_actions/scheduling.rs"]
mod scheduling;
#[path = "corporate_actions/splits.rs"]
mod splits;
#[path = "corporate_actions/support.rs"]
mod support;
#[path = "corporate_actions/timeline.rs"]
mod timeline;
