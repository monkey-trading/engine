//! Corporate actions exercise the public signed command path and Engine::apply boundaries: signed
//! scheduling, cash, splits and their rounding, calendars, missed rollovers and bounded catch-up,
//! emissions, restart and replay, with shared fixtures in `corporate_actions/support.rs`.

#[path = "corporate_actions/cash.rs"]
mod cash;
#[path = "corporate_actions/catch_up.rs"]
mod catch_up;
mod common;
#[path = "corporate_actions/emissions.rs"]
mod emissions;
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
