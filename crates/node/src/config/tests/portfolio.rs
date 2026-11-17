//! The `[portfolio_margin]` section: the dev config's fixture group loads, and a mistyped key, a
//! malformed grid or one that does not fit the genesis listings fails the boot.

use super::*;
use crate::test_support::DEV_CONFIG as DEV;

#[test]
fn the_dev_config_names_its_fixture_group() {
    let grid = NodeConfig::load(Path::new(DEV)).unwrap().portfolio_margin.expect("the dev config names a grid");
    let members: Vec<MarketId> = grid.groups.iter().flat_map(|g| g.members.iter().map(|m| m.market)).collect();
    assert_eq!((grid.groups[0].floor_share_bps, members), (5_000, vec![1, 2]));
}

#[test]
fn a_mistyped_malformed_or_misfit_grid_fails_the_boot() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    // An unknown key beside the real ones, in the section, a group and a member.
    for (from, to) in [
        ("\n[portfolio_margin]\n", "\n[portfolio_margin]\nfloor_share_bps = 1\n"),
        ("\nfloor_share_bps = 5000", "\nfloor_share_bps = 5000\nfloor_shar_bps = 1"),
        ("residual_shock_bps = 20, weekend", "residual_shock_bps = 20, residual_shok_bps = 1, weekend"),
    ] {
        let text = dev.replacen(from, to, 1);
        let refused = toml::from_str::<NodeConfig>(&text).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(refused.contains("unknown field"), "{to}: {refused}");
    }
    let member = |market: &str| {
        dev.replace("{ market = 2, loading = \"1.25\"", &format!("{{ market = {market}, loading = \"1.25\""))
    };
    for (text, expected) in [
        (
            dev.replace("floor_share_bps = 5000", "floor_share_bps = 0"),
            "group crypto: floor_share_bps must be in 1..=10000",
        ),
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
