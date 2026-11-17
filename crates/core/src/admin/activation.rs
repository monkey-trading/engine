//! The rule sets an `ActivateProtocol` starts that check for themselves whether it crosses their version,
//! before the version it names takes effect.

use crate::engine::Engine;
use crate::protocol::ProtocolVersion;

impl Engine {
    /// Starts each of those rule sets for an activation to `version`, in this order, which replay
    /// depends on; each does nothing unless the activation crosses its own version.
    pub(super) fn start_rule_sets(&mut self, version: ProtocolVersion) {
        self.start_listing_bounds(version);
        self.carry_kept_notices(version);
        self.start_toxicity_score(version);
        self.floor_bound_listings(version);
        self.end_open_markout_intervals(version);
        self.start_hold_clocks(version);
        self.start_volume_tiers(version);
        self.start_weekend_closes(version);
        self.start_house_loss(version);
        self.start_turns_from(version);
        self.start_price_rule_bounds(version);
        self.start_corporate_bounds(version);
        self.start_rollover_spans(version);
    }
}
