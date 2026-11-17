//! `engine admin`'s minimum level distance flag, from `V79`.

use engine_core::command::AdminCommand;
use engine_core::listing_bounds::ListingNotice;

use crate::admin::tests::action;

#[test]
fn announce_listing_names_a_minimum_level_distance() {
    let args = ["announce-listing", "1", "--min-level-distance-bps", "45", "--effective-at", "2027-01-01T00:00:00Z"];
    let notice = ListingNotice {
        markup_bps: None,
        min_level_distance_bps: Some(45),
        ..ListingNotice::markup(0, 1_798_761_600_000)
    };
    assert_eq!(action(&args).command().unwrap(), AdminCommand::AnnounceListing { market: 1, notice });
}
