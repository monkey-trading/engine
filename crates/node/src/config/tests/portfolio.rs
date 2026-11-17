//! The `[portfolio_margin]` section: the dev config's fixture group loads, and a mistyped key, a
//! malformed grid or one that does not fit the genesis listings fails the boot.

use super::*;
use crate::test_support::DEV_CONFIG as DEV;

#[test]
fn the_dev_config_names_its_fixture_group() {
    let grid = NodeConfig::load(Path::new(DEV)).unwrap().portfolio_margin.expect("the dev config names a grid");
    let members: Vec<MarketId> = grid.groups.iter().flat_map(|g| g.members.iter().map(|m| m.market)).collect();
    assert_eq!((grid.floor_share_bps, members), (5_000, vec![1, 2]));
}

#[test]
fn a_mistyped_malformed_or_misfit_grid_fails_the_boot() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    assert!(toml::from_str::<NodeConfig>(&dev.replace("\nfactor_shock_bps = 420", "\nfactor_shok_bps = 420")).is_err());
    let member = |market: &str| {
        dev.replace("{ market = 2, loading = \"1.05\"", &format!("{{ market = {market}, loading = \"1.05\""))
    };
    for (text, expected) in [
        (dev.replace("floor_share_bps = 5000", "floor_share_bps = 0"), "floor_share_bps must be in 1..=10000"),
        (member("1"), "market 1 is in more than one place"),
        // EUR-USD is an FX market on its own hours.
        (member("10"), "markets 1 and 10 differ in asset class or schedule"),
    ] {
        let cfg: NodeConfig = toml::from_str(&text).unwrap();
        let refused = format!("{:#}", cfg.validate().unwrap_err());
        assert!(refused.contains(expected), "{expected}: {refused}");
    }
    // A member genesis does not list counts for nothing.
    let cfg: NodeConfig = toml::from_str(&member("99")).unwrap();
    cfg.validate().unwrap();
}
