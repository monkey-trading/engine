//! `engine admin`'s portfolio margin commands, from `V81`: a grid notice and `relist-markets`.

use std::fs;

use engine_core::command::AdminCommand;

use crate::admin::tests::{SOL, action};
use crate::test_support::{dev_config, scratch_dir};

#[test]
fn a_grid_notice_takes_the_groups_file_and_checks_it_first() {
    let dir = scratch_dir("admin-grid");
    fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, text: &str| {
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path.to_string_lossy().into_owned()
    };
    let at = "2027-01-01T00:00:00Z";
    let launch = dev_config().genesis.params.portfolio_grid;
    let grid = write("grid.toml", &toml::to_string(&launch).unwrap());
    let args = ["announce-params", "--portfolio-grid", &grid, "--effective-at", at];
    let AdminCommand::AnnounceParams(notice) = action(&args).command().unwrap() else { panic!("a notice") };
    assert_eq!(notice.portfolio_grid.as_ref(), Some(&launch));
    let mut lone = launch.clone();
    lone.groups[0].members.truncate(1);
    let lone = write("lone-member.toml", &toml::to_string(&lone).unwrap());
    let err = action(&["announce-params", "--portfolio-grid", &lone, "--effective-at", at]).command().unwrap_err();
    assert!(format!("{err:#}").contains("needs at least two members"), "{err:#}");
}

#[test]
fn relist_markets_carries_every_spec_file_and_any_bad_file_refuses_all() {
    let dir = scratch_dir("admin-relist");
    fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, text: &str| {
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path.to_string_lossy().into_owned()
    };
    let markets = dev_config().genesis.markets;
    let btc = write("btc.toml", &toml::to_string(&markets[0]).unwrap());
    let eth = write("eth.toml", &toml::to_string(&markets[1]).unwrap());
    let AdminCommand::RelistMarkets { specs } = action(&["relist-markets", &btc, &eth]).command().unwrap() else {
        panic!("relist-markets relists markets")
    };
    assert_eq!(specs, markets[..2]);
    let bad = write("bad.toml", &SOL.replace("house_cap_bps = 1000", "house_cap_bps = 0"));
    assert!(action(&["relist-markets", &btc, &bad]).command().is_err());
}

#[test]
fn relist_markets_needs_at_least_one_file() {
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: crate::admin::AdminArgs,
    }
    assert!(Cli::try_parse_from(["engine", "--pin", "0x00", "relist-markets"]).is_err());
}
