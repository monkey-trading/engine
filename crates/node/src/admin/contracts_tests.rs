//! `engine admin`'s futures contract notice flag, from `V87`.

use std::fs;

use engine_core::command::AdminCommand;

use crate::admin::tests::{SOL, action};
use crate::test_support::scratch_dir;

#[test]
fn announce_listing_names_the_contracts_of_a_spec_file() {
    let dir = scratch_dir("admin-contracts-notice");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("futures.toml");
    let contracts = r#"contracts = [{ contract = "SOLZ6", from = "2026-09-10T13:30:00Z" }]"#;
    fs::write(&file, format!("{SOL}{contracts}\n")).unwrap();
    let file = file.to_string_lossy();
    let args = ["announce-listing", "3", "--contracts-from", &file, "--effective-at", "2027-01-01T00:00:00Z"];
    let AdminCommand::AnnounceListing { market: 3, notice } = action(&args).command().unwrap() else {
        panic!("announce-listing gives notice")
    };
    let named: Vec<String> = notice.contracts.expect("the file's contracts").into_iter().map(|c| c.contract).collect();
    assert_eq!((named, notice.price_rules, notice.markup_bps), (vec!["SOLZ6".to_string()], None, None));
}
