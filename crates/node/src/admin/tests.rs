use super::*;
use std::fs;

use clap::{CommandFactory, Parser};
use engine_core::market::QuoteCurrency;

use crate::test_support::{DEV_CONFIG, dev_config, scratch_dir};

/// The dev operator key, whose public key `config/dev.toml` names.
const DEV_KEY: &str = "0x4242424242424242424242424242424242424242424242424242424242424242";
/// A pin no node's key matches.
const ZERO_PIN: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";
/// A valid key that is not the dev operator's.
const OTHER_KEY: &str = "0x0101010101010101010101010101010101010101010101010101010101010101";
/// A 24/7 listing the dev genesis does not have, as an operator writes one.
const SOL: &str = r#"
id = 3
symbol = "SOL-USD"
asset_class = "crypto"
max_slippage_bps = 25
house_cap_bps = 1000
margin_tiers = [{ up_to = "1000000", bps = 1000 }, { bps = 2000 }]
spread_tiers = [{ up_to = "1000000", bps = 0 }, { up_to = "5000000", bps = 3 }, { bps = 8 }]
schedule = { kind = "always_open" }
price_rules = { max_age_ms = 1000, max_divergence_bps = 25, min_sources = 2 }
financing = { benchmark = "SOFR", markup_bps = 500, day_basis = 365 }
"#;

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    args: AdminArgs,
}

#[test]
fn the_dev_key_is_the_dev_configs_operator() {
    let key = operator_key(None, Some(HexKey(DEV_KEY.into()))).unwrap();
    assert_eq!(eip712::address(key.verifying_key()), dev_config().operator().unwrap().address);
}

#[test]
fn the_key_comes_from_the_file_before_the_environment() {
    let dir = scratch_dir("admin-key-file");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("operator.key");
    fs::write(&file, format!("{DEV_KEY}\n")).unwrap();
    let from_file = operator_key(Some(&file), Some(HexKey(OTHER_KEY.into()))).unwrap();
    assert_eq!(from_file.to_bytes(), operator_key(None, Some(HexKey(DEV_KEY.into()))).unwrap().to_bytes());
    assert_ne!(from_file.to_bytes(), operator_key(None, Some(HexKey(OTHER_KEY.into()))).unwrap().to_bytes());
}

#[test]
fn the_key_file_flag_wins_over_the_environment_then_its_file_over_its_key() {
    let dir = scratch_dir("admin-key-sources");
    fs::create_dir_all(&dir).unwrap();
    let (flag, env_file) = (dir.join("flag.key"), dir.join("env.key"));
    fs::write(&flag, DEV_KEY).unwrap();
    fs::write(&env_file, OTHER_KEY).unwrap();
    let env = || KeyEnv { key_file: Some(env_file.clone()), key: Some(HexKey(OTHER_KEY.into())) };
    let dev = operator_key(None, Some(HexKey(DEV_KEY.into()))).unwrap().to_bytes();
    let other = operator_key(None, Some(HexKey(OTHER_KEY.into()))).unwrap().to_bytes();
    assert_eq!(resolve_key(Some(&flag), env()).unwrap().to_bytes(), dev);
    let env_file_only = KeyEnv { key: Some(HexKey(DEV_KEY.into())), ..env() };
    assert_eq!(resolve_key(None, env_file_only).unwrap().to_bytes(), other);
    let key_only = KeyEnv { key_file: None, key: Some(HexKey(DEV_KEY.into())) };
    assert_eq!(resolve_key(None, key_only).unwrap().to_bytes(), dev);
}

#[tokio::test]
async fn a_key_that_is_not_the_operators_is_refused_before_any_call() {
    // Nothing listens on the port, so reaching the node would fail differently.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let node = format!("https://127.0.0.1:{port}");
    let argv = ["engine", "--config", DEV_CONFIG, "--node", &node, "--pin", ZERO_PIN, "set-reduce-only", "true"];
    let args = Cli::try_parse_from(argv).unwrap().args;
    let env = KeyEnv { key_file: None, key: Some(HexKey(OTHER_KEY.into())) };
    let err = run_with_clock(args, env, || 1_000).await.unwrap_err();
    assert!(err.to_string().contains("is not the operator key"), "{err:#}");
}

#[tokio::test]
async fn a_rotated_key_is_signed_with_once_the_flag_says_the_log_names_it() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let node = format!("https://127.0.0.1:{port}");
    let argv =
        ["engine", "--config", DEV_CONFIG, "--node", &node, "--pin", ZERO_PIN, "--rotated", "set-reduce-only", "true"];
    let args = Cli::try_parse_from(argv).unwrap().args;
    let env = KeyEnv { key_file: None, key: Some(HexKey(OTHER_KEY.into())) };
    // The key check passes, so the run reaches the (absent) node.
    let err = run_with_clock(args, env, || 1_000).await.unwrap_err();
    assert!(!err.to_string().contains("is not the operator key"), "{err:#}");
}

#[test]
fn a_key_never_prints() {
    let key = HexKey(DEV_KEY.into());
    assert!(!format!("{key:?} {key}").contains("4242"));
}

#[test]
fn a_missing_or_malformed_key_is_refused_without_echoing_it() {
    let err = operator_key(None, None).unwrap_err();
    assert!(err.to_string().contains("OPERATOR_KEY"), "{err:#}");
    let short = "0x4242424242";
    for bad in [short, "not hex at all", "0x0000000000000000000000000000000000000000000000000000000000000000"] {
        let err = format!("{:#}", operator_key(None, Some(HexKey(bad.into()))).unwrap_err());
        assert!(err.contains("not a secp256k1 private key") && !err.contains(bad), "{err}");
    }
    let err = operator_key(Some(Path::new("/nonexistent/operator.key")), Some(HexKey(DEV_KEY.into()))).unwrap_err();
    assert!(err.to_string().contains("/nonexistent/operator.key"), "{err:#}");
}

#[test]
fn no_argument_takes_the_key_itself() {
    let mut names: Vec<String> = Cli::command().get_arguments().map(|arg| arg.get_id().to_string()).collect();
    names.sort();
    assert_eq!(names, ["config", "key_file", "node", "pin", "rotated"]);
    let with_key = ["engine", "--pin", ZERO_PIN, "--key", DEV_KEY, "set-reduce-only", "true"];
    assert!(Cli::try_parse_from(with_key).is_err());
}

#[test]
fn each_action_builds_its_admin_command() {
    let cases = [
        (vec!["set-reduce-only", "true"], AdminCommand::SetReduceOnly { market: None, reduce_only: true }),
        (
            vec!["set-reduce-only", "false", "-m", "2"],
            AdminCommand::SetReduceOnly { market: Some(2), reduce_only: false },
        ),
        (vec!["set-stress-floor", "1"], AdminCommand::SetStressFloor { market: None, level: 1 }),
        (vec!["set-stress-floor", "2", "--market", "1"], AdminCommand::SetStressFloor { market: Some(1), level: 2 }),
        (
            vec!["set-benchmark-rate", "SOFR", "-25"],
            AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: -25 },
        ),
        (
            vec!["announce-params", "--profit-fee-bps", "1200", "--effective-at", "2027-01-01T00:00:00Z"],
            AdminCommand::AnnounceParams(ParamsNotice {
                profit_fee_bps: Some(1_200),
                closeout_level_bps: None,
                effective_at: 1_798_761_600_000,
            }),
        ),
        (
            vec!["rotate-operator", "0x5555555555555555555555555555555555555555"],
            AdminCommand::RotateOperator { address: AccountId([0x55; 20]) },
        ),
        (
            vec!["init-emissions", "--start", "2027-01-01T00:00:00Z", "--period-budget", "1000000000000000000000"],
            AdminCommand::InitEmissions(EmissionSchedule {
                start_ms: 1_798_761_600_000,
                period_budget: TokenUnits(1_000_000_000_000_000_000_000),
            }),
        ),
    ];
    for (args, expected) in cases {
        assert_eq!(action(&args).command().unwrap(), expected, "{args:?}");
    }
    assert!(Cli::try_parse_from(["engine", "--pin", ZERO_PIN, "set-reduce-only", "on"]).is_err());
    // Both emission values are required, and a budget is base units in decimal digits.
    for args in [
        vec!["init-emissions", "--start", "2027-01-01T00:00:00Z"],
        vec!["init-emissions", "--period-budget", "1"],
        vec!["init-emissions", "--start", "2027-01-01", "--period-budget", "1"],
        vec!["init-emissions", "--start", "2027-01-01T00:00:00Z", "--period-budget", "1.5"],
    ] {
        let argv = ["engine", "--pin", ZERO_PIN].into_iter().chain(args.iter().copied());
        assert!(Cli::try_parse_from(argv).is_err(), "{args:?}");
    }
}

#[test]
fn a_listing_or_parameters_come_from_toml_and_are_validated_before_signing() {
    let dir = scratch_dir("admin-toml");
    fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, text: &str| {
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path.to_string_lossy().into_owned()
    };
    let sol = write("sol.toml", SOL);
    let AdminCommand::UpsertMarket(spec) = action(&["upsert-market", &sol]).command().unwrap() else {
        panic!("upsert-market lists a market")
    };
    assert_eq!((spec.id, spec.symbol.as_str()), (3, "SOL-USD"));

    let params = toml::to_string(&dev_config().genesis.params).unwrap();
    let params = write("params.toml", &params);
    let expected = AdminCommand::SetParams(dev_config().genesis.params);
    assert_eq!(action(&["set-params", &params]).command().unwrap(), expected);

    let mut wide = dev_config().genesis.params;
    let top = wide.stress_levels.len() - 1;
    wide.stress_levels[top].min_spread_bps = 10_000;
    let wide = write("wide.toml", &toml::to_string(&wide).unwrap());
    let err = action(&["set-params", &wide]).command().unwrap_err();
    assert!(format!("{err:#}").contains("reach the price"), "{err:#}");
    // The latest version takes a divergence trigger only as a share of each market's bound.
    let absolute = toml::to_string(&dev_config().genesis.params)
        .unwrap()
        .replace("divergence_share_bps = 6000", "divergence_bps = 15");
    let absolute = write("absolute.toml", &absolute);
    let err = action(&["set-params", &absolute]).command().unwrap_err();
    assert!(format!("{err:#}").contains("divergence_share_bps"), "{err:#}");

    let invalid = write("invalid.toml", &SOL.replace("house_cap_bps = 1000", "house_cap_bps = 0"));
    let mistyped = write("mistyped.toml", &SOL.replace("house_cap_bps", "house_capp_bps"));
    for file in [invalid, mistyped, dir.join("missing.toml").to_string_lossy().into_owned()] {
        assert!(action(&["upsert-market", &file]).command().is_err(), "{file}");
    }
}

#[test]
fn parameters_without_notional_weighting_are_refused_before_signing() {
    // The latest version weighs markouts by notional, so parameters that name no weighting never
    // run; the client refuses them before any call, as it does other invalid files.
    let dir = scratch_dir("admin-no-notional");
    fs::create_dir_all(&dir).unwrap();
    let mut unweighted = dev_config().genesis.params;
    unweighted.toxicity.notional = None;
    let file = dir.join("unweighted.toml");
    fs::write(&file, toml::to_string(&unweighted).unwrap()).unwrap();
    let err = action(&["set-params", file.to_str().unwrap()]).command().unwrap_err();
    assert!(format!("{err:#}").contains("toxicity needs notional weighting"), "{err:#}");
}

#[test]
fn a_listing_or_parameters_carry_cap_floors_and_a_quote_currency_from_toml() {
    let dir = scratch_dir("admin-floors");
    fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, text: &str| {
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path.to_string_lossy().into_owned()
    };
    let euro = "house_cap_floor = \"1000000\"\nquote_currency = { currency = \"eur\", fx_market = 10 }\n";
    let de40 = write("de40.toml", &format!("{euro}{}", SOL.replace("id = 3", "id = 20")));
    let AdminCommand::UpsertMarket(spec) = action(&["upsert-market", &de40]).command().unwrap() else {
        panic!("upsert-market lists a market")
    };
    assert_eq!(spec.house_cap_floor, "1000000".parse().unwrap());
    assert_eq!(spec.quote_currency, QuoteCurrency::Eur { fx_market: 10 });

    let mut params = dev_config().genesis.params;
    params.house_total_cap_floor = "5000000".parse().unwrap();
    let floored = write("floored.toml", &toml::to_string(&params).unwrap());
    assert_eq!(action(&["set-params", &floored]).command().unwrap(), AdminCommand::SetParams(params.clone()));

    // Floors below zero are refused before anything is signed.
    let negative = write("negative.toml", &format!("house_cap_floor = \"-1\"\n{SOL}"));
    params.house_total_cap_floor = "-1".parse().unwrap();
    let below = write("below.toml", &toml::to_string(&params).unwrap());
    for (kind, file) in [("upsert-market", negative), ("set-params", below)] {
        let err = format!("{:#}", action(&[kind, &file]).command().unwrap_err());
        assert!(err.contains("floor out of range"), "{kind}: {err}");
    }
}

#[test]
fn a_signed_request_carries_the_operators_signature_over_its_command() {
    let operator = dev_config().operator().unwrap();
    let key = operator_key(None, Some(HexKey(DEV_KEY.into()))).unwrap();
    let command = AdminCommand::SetReduceOnly { market: None, reduce_only: true };
    let req = sign(&key, &operator, command.clone(), 4, 1_000);
    assert_eq!((req.command.clone(), req.auth.nonce, req.auth.expires_at), (command, 4, 1_000));
    assert_eq!(operator.check(&req.command, &req.auth), Ok(()));
    let elsewhere = Operator { chain_id: operator.chain_id + 1, ..operator };
    assert!(elsewhere.check(&req.command, &req.auth).is_err());
}

#[test]
fn the_socket_target_drops_ipv6_brackets_and_defaults_to_port_443() {
    let cases = [
        ("https://127.0.0.1:8080", ("127.0.0.1", 8080)),
        ("https://[::1]:8080", ("::1", 8080)),
        ("https://[2001:db8::1]", ("2001:db8::1", 443)),
        ("https://localhost", ("localhost", 443)),
        ("https://node.example:9000", ("node.example", 9000)),
    ];
    for (url, expected) in cases {
        let uri: Uri = url.parse().unwrap();
        assert_eq!(socket_target(uri.authority().unwrap()), expected, "{url}");
    }
}

#[tokio::test]
async fn a_node_url_that_is_not_https_host_port_or_does_not_answer_is_an_error() {
    let req = sign(
        &operator_key(None, Some(HexKey(DEV_KEY.into()))).unwrap(),
        &dev_config().operator().unwrap(),
        AdminCommand::SetStressFloor { market: None, level: 1 },
        0,
        1_000,
    );
    let node = |url: &str| NodeApi { url: url.parse().unwrap(), pin: [0; 32] };
    for url in ["http://127.0.0.1:8080", "https://127.0.0.1:8080/engine", "https://127.0.0.1:8080/?x=1"] {
        let err = submit(&node(url), &req).await.unwrap_err();
        assert!(err.to_string().contains("https://host:port"), "{url}: {err:#}");
    }
    // Bind a port and close it, so nothing listens there.
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let err = submit(&node(&format!("https://127.0.0.1:{port}")), &req).await.unwrap_err();
    assert!(format!("{err:#}").contains("POST https://127.0.0.1"), "{err:#}");
}

#[test]
fn a_pin_is_the_attested_key_hash_in_hex() {
    let hash = [0xab; 32];
    assert_eq!(tls::parse_pin(&format!("0x{}", hex::encode(hash))), Ok(hash));
    assert_eq!(tls::parse_pin(&hex::encode(hash)), Ok(hash));
    for bad in ["", "0xab", &hex::encode([0xab; 33]), "not hex"] {
        assert!(tls::parse_pin(bad).is_err(), "{bad}");
    }
    assert!(Cli::try_parse_from(["engine", "set-reduce-only", "true"]).is_err(), "the pin is required");
}

#[test]
fn corporate_action_file_requires_explicit_withholding_and_validates_its_range() {
    let dir = scratch_dir("corporate-admin-input");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("action.toml");
    let input = "id = 'dividend'\nmarket = 40\nprevious_close = 1792526400000\nex_open = 1792589400000\n[adjustment]\nkind = 'dividend'\ngross_per_unit = '4'\n";
    fs::write(&path, input).unwrap();
    let read = || action(&["schedule-corporate-action", path.to_str().unwrap()]).command();
    assert!(format!("{:#}", read().unwrap_err()).contains("withholding_rate"));
    for (rate, valid) in [("0", true), ("1", true), ("0.25", true), ("-0.01", false), ("1.01", false)] {
        fs::write(&path, format!("{input}withholding_rate = '{rate}'\n")).unwrap();
        match read() {
            Ok(AdminCommand::ScheduleCorporateAction(action)) if valid => {
                assert_eq!(
                    action.adjustment,
                    engine_core::corporate::Adjustment::Dividend {
                        gross_per_unit: "4".parse().unwrap(),
                        withholding_rate: rate.parse().unwrap(),
                    }
                );
            }
            Err(err) if !valid => assert!(err.to_string().contains("withholding_rate"), "{err:#}"),
            other => panic!("rate={rate}, valid={valid}: {other:?}"),
        }
    }
}

// ── Helpers ──

/// The action `engine admin <args>` parses to.
fn action(args: &[&str]) -> AdminAction {
    let argv = ["engine", "--pin", ZERO_PIN].into_iter().chain(args.iter().copied());
    Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{args:?}: {e}")).args.action
}
