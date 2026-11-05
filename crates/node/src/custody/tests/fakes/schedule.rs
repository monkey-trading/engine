//! The fake chain's operator schedule binding: the schedule its registry names, and the registry
//! that schedule records.

use alloy::primitives::Address;

/// What the fake chain answers for the schedule binding reads.
#[derive(Clone, Copy, Default)]
pub(in crate::custody::tests) struct Schedule {
    /// The schedule `registry_schedule` answers: the zero address, the default, while the registry
    /// names none.
    pub(in crate::custody::tests) named: Address,
    /// The registry `schedule_registry` answers: `None`, the default, where the schedule has no code.
    pub(in crate::custody::tests) registry: Option<Address>,
}
