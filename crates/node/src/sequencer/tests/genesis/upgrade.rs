//! Upgrading a log that predates close-spread ceilings to `V40`: the carry, its refusals, and
//! the config the node restarts under.

use super::*;

#[test]
fn an_old_log_with_markets_takes_the_configs_ceilings_before_activating_v40() {
    // A log whose genesis predates ceilings lists markets with none adopted: recovery under the
    // full current config accepts its genesis (the install command it never ran is not part of
    // what the log attests), and activation carries the config's ceilings, filtered to the markets
    // the log lists, before the activation applies.
    let dir = scratch_dir("activation-carries-ceilings");
    let mut genesis = dev_config().genesis;
    genesis.close_spread_ceilings.clear();
    let legacy: Vec<_> = genesis_commands(&genesis)
        .into_iter()
        .filter(|cmd| !matches!(cmd, Command::Admin(AdminCommand::SetCloseSpreadCeilings { .. })))
        .collect();
    old_genesis_log(&dir, &signed(31_337), ProtocolVersion::V23, legacy, Vec::new());

    // Recovery under the dev config, whose genesis sequences the install, accepts the log.
    let mut seq = recover(&dir, &signed(31_337));
    seq.activate_protocol().unwrap();
    drop(seq);
    let replayed = replay(&log_entries(&dir)).unwrap();
    let ceilings = replayed.engine.close_spread_ceilings().expect("the carry installs the registry");
    assert_eq!(ceilings.len(), replayed.engine.markets().len());
    assert!(replayed.engine.markets().keys().all(|id| ceilings.contains_key(id)));
    assert!(replayed.activations.iter().any(|(version, _)| *version == ProtocolVersion::V40));
}

#[test]
fn an_old_log_with_a_market_the_config_ceilingless_refuses_activation_naming_it() {
    // The carry installs only a complete registry: a listed market the config names no ceiling
    // for refuses the activation, naming the market, rather than activating an uncapped log.
    let dir = scratch_dir("activation-ceilingless-market");
    let mut genesis = dev_config().genesis;
    genesis.close_spread_ceilings.clear();
    // A market the config's ceilings never name, listed in the old genesis.
    genesis.markets.push(genesis.markets[0].clone());
    let extra = genesis.markets.last_mut().unwrap();
    extra.id = 77;
    extra.symbol = "TEST77".into();
    let legacy: Vec<_> = genesis_commands(&genesis)
        .into_iter()
        .filter(|cmd| !matches!(cmd, Command::Admin(AdminCommand::SetCloseSpreadCeilings { .. })))
        .collect();
    old_genesis_log(&dir, &signed(31_337), ProtocolVersion::V23, legacy, Vec::new());

    // The config under which the node restarts names the same genesis — market 77 included — but
    // no ceiling for it: recovery accepts the old genesis, and the activation refuses, naming 77.
    let ran = genesis_commands(&genesis);
    let err = recover_with(&dir, &signed(31_337), ran).unwrap().activate_protocol().unwrap_err();
    assert!(format!("{err:#}").contains("without a close-spread ceiling"), "{err:#}");
}

#[test]
fn an_empty_legacy_genesis_takes_an_empty_registry_and_refuses_uncapped_listings() {
    // A log whose genesis listed no markets predates ceilings entirely: the upgrade still
    // installs the (empty) registry, so a listing without an adopted ceiling is refused from
    // `V40` on.
    let dir = scratch_dir("activation-empty-legacy");
    let mut genesis = dev_config().genesis;
    genesis.markets.clear();
    genesis.close_spread_ceilings.clear();
    let legacy: Vec<_> = genesis_commands(&genesis)
        .into_iter()
        .filter(|cmd| !matches!(cmd, Command::Admin(AdminCommand::SetCloseSpreadCeilings { .. })))
        .collect();
    old_genesis_log(&dir, &signed(31_337), ProtocolVersion::V23, legacy.clone(), Vec::new());

    let mut seq = recover_with(&dir, &signed(31_337), legacy).unwrap();
    seq.activate_protocol().unwrap();
    let operator = dev_config().operator().unwrap();
    let key = k256::ecdsa::SigningKey::from_slice(&[0x42; 32]).unwrap();
    let spec = dev_config().genesis.markets[0].clone();
    let listing = Command::SignedAdmin(Box::new(crate::admin::sign(
        &key,
        &operator,
        AdminCommand::UpsertMarket(spec),
        0,
        trusted_now().latest_ms + 60_000,
    )));
    let events = seq.stamp(trusted_now().latest_ms, listing).unwrap();
    assert!(rejection(&events).is_some_and(|reason| reason.contains("no adopted close-spread ceiling")), "{events:?}");
}

#[test]
fn an_old_log_with_a_post_genesis_listing_takes_its_ceiling_from_the_config() {
    // A legacy log listed a market after its genesis; the recorded genesis cannot be restated, so
    // the config names the market's ceiling beside the genesis ones: recovery accepts the old
    // genesis, and the carry covers every listed market.
    let dir = scratch_dir("activation-post-genesis-listing");
    let genesis = dev_config().genesis;
    let mut spec77 = genesis.markets[0].clone();
    spec77.id = 77;
    spec77.symbol = "TEST77".into();
    let operator = dev_config().operator().unwrap();
    let key = k256::ecdsa::SigningKey::from_slice(&[0x42; 32]).unwrap();
    let listing = Command::SignedAdmin(Box::new(crate::admin::sign(
        &key,
        &operator,
        AdminCommand::UpsertMarket(spec77),
        0,
        trusted_now().latest_ms + 60_000,
    )));
    let legacy: Vec<_> = genesis_commands(&genesis)
        .into_iter()
        .filter(|cmd| !matches!(cmd, Command::Admin(AdminCommand::SetCloseSpreadCeilings { .. })))
        .collect();
    old_genesis_log(&dir, &signed(31_337), ProtocolVersion::V23, legacy, vec![listing]);

    let mut ceilings = genesis.close_spread_ceilings.clone();
    ceilings.push(CloseSpreadCeiling { market: 77, max_close_spread_bps: 25 });
    let cfg = SequencerConfig {
        client_auth: signed(31_337),
        genesis: genesis_commands(&genesis),
        close_spread_ceilings: ceilings,
        ..sequencer_config(&dir, tokio::sync::watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(
        cfg,
        Arc::new(EnclaveKey::test_key()),
        broadcast::channel(16).0,
        mpsc::unbounded_channel().0,
    )
    .unwrap();
    seq.trust(trusted_now()).unwrap();
    seq.activate_protocol().unwrap();
    drop(seq);
    let replayed = replay(&log_entries(&dir)).unwrap();
    let registry = replayed.engine.close_spread_ceilings().expect("the carry installs the registry");
    assert!(replayed.engine.markets().keys().all(|id| registry.contains_key(id)));
    assert_eq!(registry[&77], 25);
}

#[test]
fn a_signed_adoption_after_the_carry_survives_restart_under_the_unchanged_config() {
    // The drift check compares only what the config names: a market's ceiling the log later
    // adopted through the signed notice needs no config entry, and a restart under the upgrade's
    // unchanged config accepts it.
    let dir = scratch_dir("activation-adoption-restart");
    let mut genesis = dev_config().genesis;
    genesis.close_spread_ceilings.clear();
    let legacy: Vec<_> = genesis_commands(&genesis)
        .into_iter()
        .filter(|cmd| !matches!(cmd, Command::Admin(AdminCommand::SetCloseSpreadCeilings { .. })))
        .collect();
    old_genesis_log(&dir, &signed(31_337), ProtocolVersion::V23, legacy.clone(), Vec::new());
    let mut seq = recover_with(&dir, &signed(31_337), legacy).unwrap();
    seq.activate_protocol().unwrap();
    let operator = dev_config().operator().unwrap();
    let key = k256::ecdsa::SigningKey::from_slice(&[0x42; 32]).unwrap();
    let notice_ms: Millis = 7 * 24 * 60 * 60 * 1000;
    let ts = trusted_now().latest_ms;
    let adopt = Command::SignedAdmin(Box::new(crate::admin::sign(
        &key,
        &operator,
        AdminCommand::AdoptCloseSpreadCeiling { market: 77, max_close_spread_bps: 25, effective_at: ts + notice_ms },
        0,
        ts + 60_000,
    )));
    let events = seq.stamp(ts, adopt).unwrap();
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    drop(seq);

    let mut seq = recover_with(&dir, &signed(31_337), genesis_commands(&dev_config().genesis)).unwrap();
    seq.activate_protocol().unwrap();
    // The tick after the notice lets the adoption apply; its catch-up events do not matter here.
    seq.stamp(ts + notice_ms + 1, Command::Tick).unwrap();
    drop(seq);
    let replayed = replay(&log_entries(&dir)).unwrap();
    assert_eq!(replayed.engine.close_spread_ceilings().unwrap()[&77], 25);
    // Recovery with the adopted entry in force, under the upgrade's unchanged config: the check
    // compares only what the config names, so the adoption passes.
    let mut seq = recover_with(&dir, &signed(31_337), genesis_commands(&dev_config().genesis)).unwrap();
    seq.activate_protocol().unwrap();
    drop(seq);
    let replayed = replay(&log_entries(&dir)).unwrap();
    assert_eq!(replayed.engine.close_spread_ceilings().unwrap()[&77], 25);
}

#[test]
fn a_config_that_sits_below_a_carried_ceiling_is_refused_at_recovery() {
    // The upgrade carries the config's ceilings into the log; a later restart under a config that
    // lowered one below what the log enforces is refused, naming the market, rather than attesting
    // ceilings stricter than it applies. A signed lowering under the config's value stays legal.
    let dir = scratch_dir("activation-ceiling-drift");
    let mut genesis = dev_config().genesis;
    genesis.close_spread_ceilings.clear();
    let legacy: Vec<_> = genesis_commands(&genesis)
        .into_iter()
        .filter(|cmd| !matches!(cmd, Command::Admin(AdminCommand::SetCloseSpreadCeilings { .. })))
        .collect();
    old_genesis_log(&dir, &signed(31_337), ProtocolVersion::V23, legacy.clone(), Vec::new());
    let mut seq = recover_with(&dir, &signed(31_337), legacy).unwrap();
    seq.activate_protocol().unwrap();
    drop(seq);

    let ceilings: Vec<_> = dev_config()
        .genesis
        .close_spread_ceilings
        .iter()
        .map(|ceiling| CloseSpreadCeiling {
            market: ceiling.market,
            max_close_spread_bps: if ceiling.market == 1 { 20 } else { ceiling.max_close_spread_bps },
        })
        .collect();
    let cfg = SequencerConfig {
        client_auth: signed(31_337),
        genesis: genesis_commands(&dev_config().genesis),
        close_spread_ceilings: ceilings,
        ..sequencer_config(&dir, tokio::sync::watch::channel(Manifest::default()).0)
    };
    let result = Sequencer::recover(
        cfg,
        Arc::new(EnclaveKey::test_key()),
        broadcast::channel(16).0,
        mpsc::unbounded_channel().0,
    );
    let err = result.err().expect("recovery refuses the drifted config");
    assert!(format!("{err:#}").contains("market 1"), "{err:#}");
}
