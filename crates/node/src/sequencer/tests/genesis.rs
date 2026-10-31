//! Genesis is all or nothing: a genesis the engine would reject writes nothing, and recovery refuses
//! a log whose genesis did not reach the checkpoint after its house capital credit, or is not the
//! config's.

use super::*;

use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::params::NotionalWeighting;

use crate::config::{CustodyProvider, Genesis, NodeConfig};
use crate::sequencer::fixture_tests::{geneses_no_config_can_name, old_genesis_log};
use crate::sequencer::genesis::rejection;
use crate::test_support::ANVIL_CONFIG;

#[test]
fn a_benchmark_the_engine_rejects_writes_no_genesis_and_the_fixed_config_then_lists_every_market() {
    let dir = scratch_dir("genesis-rejected-benchmark");
    let mut rejected = dev_config().genesis;
    rejected.benchmarks[0].annual_bps = 40_000;
    let mut seq = recover(&dir, &signed(31_337));
    seq.activate_protocol().unwrap();
    let activated = log_entries(&dir);
    let err = seq.genesis(&rejected).unwrap_err();
    assert!(format!("{err:#}").contains("genesis benchmark SOFR: the engine rejects it"), "{err:#}");
    drop(seq);
    assert_eq!(log_entries(&dir), activated, "no genesis command reached the log");

    let genesis = dev_config().genesis;
    let mut seq = recover(&dir, &signed(31_337));
    assert!(seq.needs_genesis());
    seq.genesis(&genesis).unwrap();
    drop(seq);
    let replayed = replay(&log_entries(&dir)).unwrap();
    assert_eq!(replayed.engine.markets().len(), genesis.markets.len());
    assert_eq!(replayed.engine.house_cash(), genesis.house_capital);
    recover(&dir, &signed(31_337));
}

#[test]
fn a_genesis_entry_of_any_kind_the_engine_would_reject_queues_and_writes_nothing() {
    type Edit = fn(&mut Genesis);
    // The first command, ones in the middle, and the last.
    let cases: [(&str, Edit); 7] = [
        ("genesis.params: the engine rejects it: profit_fee_bps", |g| g.params.profit_fee_bps = 10_000),
        ("genesis benchmark SONIA: the engine rejects it", |g| g.benchmarks[1].annual_bps = i64::MIN),
        ("genesis market EUR-USD: the engine rejects it: margin_tiers is empty", |g| g.markets[2].margin_tiers.clear()),
        ("genesis market AAPL: the engine rejects it: house_cap_bps", |g| {
            g.markets.last_mut().unwrap().house_cap_bps = 0
        }),
        ("genesis.close_spread_ceilings: the engine rejects it: market 1 is listed without", |g| {
            g.close_spread_ceilings.remove(0);
        }),
        ("genesis.house_capital: the engine rejects it", |g| g.house_capital = Fixed::from_int(-1)),
        ("genesis.house_capital: the engine rejects it", |g| g.house_capital = Fixed::MAX_INPUT + Fixed::from_int(1)),
    ];
    for (n, (expected, edit)) in cases.into_iter().enumerate() {
        let mut genesis = dev_config().genesis;
        edit(&mut genesis);
        // Waiting for trusted time, as at boot, so a genesis that queued would go in on the sample.
        let dir = scratch_dir(&format!("genesis-rejected-{n}"));
        let mut seq = try_recover(&dir, &signed(31_337)).0.unwrap();
        seq.activate_protocol().unwrap();
        let err = seq.genesis(&genesis).unwrap_err();
        assert!(format!("{err:#}").contains(expected), "expected {expected:?}, got {err:#}");
        seq.trust(trusted_now()).unwrap();
        let written = commands(&log_entries(&dir));
        let activation = |cmd: &Command| match cmd {
            Command::Admin(AdminCommand::SetClientAuth(_) | AdminCommand::SetOperator(_)) => true,
            Command::Admin(AdminCommand::ActivateCustodyFinality { version, legacy_deposits }) => {
                *version == ProtocolVersion::LATEST && legacy_deposits.is_empty()
            }
            _ => false,
        };
        assert!(!written.is_empty() && written.iter().all(activation), "{expected}: {written:?}");
        assert!(seq.needs_genesis(), "{expected}");
    }
}

#[test]
fn a_genesis_staking_start_stale_by_admission_is_stamped_refused_and_recovery_refuses() {
    // Config load's genesis check stamps at zero, so a staking start only milliseconds old passes
    // it; admission stamps trusted time, and the refusal is stamped and stops the sequencer, so
    // recovery names the seq the engine rejected.
    let dir = scratch_dir("genesis-staking-stale-start");
    let mut genesis = dev_config().genesis;
    genesis.staking = Some(engine_core::staking::StakingConfig {
        liquidity_floor: Fixed::from_int(1),
        epoch_ms: 60_000,
        start_ms: crate::clock::now_ms() - 3_600_000,
        cooldown_ms: 86_400_000,
    });
    let mut seq = try_recover(&dir, &signed(31_337)).0.unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&genesis).unwrap();
    let err = seq.trust(trusted_now()).unwrap_err();
    assert!(format!("{err:#}").contains("genesis: rejected: staking start"), "{err:#}");
    drop(seq);
    let err = try_recover(&dir, &signed(31_337)).0.err().expect("refused");
    assert!(format!("{err:#}").contains("the engine rejected seq"), "{err:#}");
}

#[test]
fn a_genesis_cut_short_is_refused_at_recovery_and_nothing_is_written() {
    // Under mock custody with house capital, and under EVM custody with none, a log cut anywhere
    // between its first genesis command and the checkpoint after the credit is refused; one holding
    // only protocol activation runs genesis.
    let anvil = NodeConfig::load(Path::new(ANVIL_CONFIG)).unwrap();
    assert_eq!(anvil.custody.provider, CustodyProvider::Evm);
    for (config, genesis) in [("dev", dev_config().genesis), ("anvil", anvil.genesis)] {
        let all = genesis_commands(&genesis);
        for cut in [0, 1, all.len() / 2, all.len() - 1, all.len()] {
            let name = format!("{config}-cut-{cut}");
            let dir = scratch_dir(&format!("genesis-{name}"));
            let mut seq = recover(&dir, &signed(31_337));
            seq.activate_protocol().unwrap();
            seq.sequence_required("genesis", all[..cut].to_vec()).unwrap();
            drop(seq);
            if cut == all.len() {
                // As if the process died after the credit's line reached disk and before its
                // checkpoint, whether or not a scheduled checkpoint shares its seq.
                let segment = last_segment(&dir);
                let text = fs::read_to_string(&segment).unwrap();
                let credit = text.find(r#""cmd":{"type":"house_capital""#).expect("the credit is in the last segment");
                fs::write(&segment, &text[..credit + text[credit..].find('\n').unwrap() + 1]).unwrap();
            }
            let before = log_entries(&dir);

            match try_recover(&dir, &signed(31_337)).0 {
                Ok(mut seq) if cut == 0 => {
                    assert!(seq.needs_genesis(), "{name}");
                    seq.trust(trusted_now()).unwrap();
                    seq.genesis(&genesis).unwrap();
                    drop(seq);
                    let replayed = replay(&log_entries(&dir)).unwrap();
                    assert_eq!(replayed.engine.markets().len(), genesis.markets.len(), "{name}");
                    replayed.genesis.ensure_closed().unwrap();
                }
                Ok(_) => panic!("{name}: a log cut inside genesis recovered"),
                Err(err) => {
                    assert_ne!(cut, 0, "{name}: {err:#}");
                    let expected = "did not reach the checkpoint after its house capital credit";
                    assert!(format!("{err:#}").contains(expected), "{name}: {err:#}");
                    assert!(format!("{err:#}").contains("remove node.log_dir"), "{name}: {err:#}");
                    assert_eq!(log_entries(&dir), before, "{name}: recovery wrote to the log");
                }
            }
        }
    }
}

#[test]
fn a_genesis_an_earlier_build_ran_again_is_refused_and_a_fresh_log_advised_only_when_nothing_follows() {
    // Earlier builds ran genesis again in full on the next boot, after a first run the engine
    // rejected part of or a crash cut short, and then served whatever came next.
    let genesis = dev_config().genesis;
    let all = genesis_commands(&genesis);
    let mut rejected = all[..2].to_vec();
    rejected[1] = Command::Admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 40_000 });
    // Each first run, how the sequencer ended it, and the flaw recovery then finds.
    let first_runs = [
        (rejected, Some("genesis: rejected: benchmark rate 40000bps"), "the engine rejected seq 4"),
        (all[..all.len() / 2].to_vec(), None, "it ran again from seq"),
    ];
    for (n, (first_run, stopped, flaw)) in first_runs.into_iter().enumerate() {
        for followed in [false, true] {
            let name = format!("run-{n}-followed-{followed}");
            let dir = scratch_dir(&format!("genesis-ran-again-{name}"));
            let mut seq = recover(&dir, &signed(31_337));
            seq.activate_legacy_protocol().unwrap();
            match (seq.sequence_required("genesis", first_run.clone()), stopped) {
                (Err(err), Some(stopped)) => assert!(format!("{err:#}").contains(stopped), "{name}: {err:#}"),
                (Ok(()), None) => {}
                (outcome, _) => panic!("{name}: the first run ended {outcome:?}"),
            }
            seq.sequence_required("genesis", all.clone()).unwrap();
            if followed {
                seq.sequence(deposit(100, "dep-1")).unwrap();
            }
            seq.flush().unwrap();
            drop(seq);
            let before = log_entries(&dir);
            assert_eq!(replay(&before).unwrap().engine.markets().len(), genesis.markets.len(), "{name}");

            let err = format!("{:#}", try_recover(&dir, &signed(31_337)).0.err().expect("refused"));
            assert!(err.contains(&format!("genesis did not apply in full: {flaw}")), "{name}: {err}");
            let advice = if followed { "keep it for an operator to examine" } else { "remove node.log_dir" };
            assert!(err.contains(advice), "{name}: {err}");
            assert_eq!(log_entries(&dir), before, "{name}: recovery wrote to the log");
        }
    }
}

#[test]
fn a_required_command_the_engine_rejects_stops_the_sequencer_at_the_first_sample() {
    // Queued before trusted time admits a stamp, as at boot, and sequenced by the first sample.
    let dir = scratch_dir("genesis-rejected-at-sample");
    let mut seq = try_recover(&dir, &signed(31_337)).0.unwrap();
    seq.activate_protocol().unwrap();
    let rejected = Command::Admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 40_000 });
    seq.sequence_required("genesis", vec![rejected]).unwrap();
    assert!(commands(&log_entries(&dir)).is_empty(), "nothing before the first sample");

    let err = seq.trust(trusted_now()).unwrap_err();
    assert!(format!("{err:#}").contains("genesis: rejected: benchmark rate 40000bps out of range"), "{err:#}");
    drop(seq);
    // The rejected group gets no checkpoint, and recovery refuses the genesis it started.
    let written = log_entries(&dir);
    assert!(!matches!(written.last(), Some(LogEntry::Checkpoint(_))), "{written:?}");
    let err = try_recover(&dir, &signed(31_337)).0.err().expect("refused");
    assert!(format!("{err:#}").contains("the engine rejected seq"), "{err:#}");
}

#[test]
fn an_old_log_with_markets_takes_the_configs_ceilings_before_activating_v39() {
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
    assert!(replayed.activations.iter().any(|(version, _)| *version == ProtocolVersion::V39));
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
    // `V39` on.
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
    let mut genesis = dev_config().genesis;
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
fn a_restart_under_another_genesis_refuses_to_start_and_writes_nothing() {
    let dir = scratch_dir("genesis-mismatch");
    let genesis = dev_config().genesis;
    let mut seq = recover(&dir, &signed(31_337));
    seq.activate_protocol().unwrap();
    seq.genesis(&genesis).unwrap();
    drop(seq);
    let written = log_entries(&dir);

    // The genesis it ran restarts; any edit to the config's genesis is refused.
    assert!(!recover(&dir, &signed(31_337)).needs_genesis());
    let edits = [
        ("house capital", (|g: &mut Genesis| g.house_capital += Fixed::ONE) as fn(&mut Genesis)),
        ("benchmark rate", |g| g.benchmarks[0].annual_bps += 1),
        ("one more market", |g| g.markets.push(g.markets[0].clone())),
        ("one market fewer", |g| drop(g.markets.pop())),
    ];
    for (what, edit) in edits {
        let mut other = dev_config().genesis;
        edit(&mut other);
        let err = recover_with(&dir, &signed(31_337), genesis_commands(&other)).err().unwrap();
        assert!(format!("{err:#}").contains("differs from the config's [genesis]"), "{what}: {err:#}");
    }
    assert_eq!(log_entries(&dir), written);
}

#[test]
fn a_genesis_set_before_the_notional_weighting_takes_the_one_the_node_carried() {
    // A log from before `V11`, whose genesis parameters have no weighting, resumes under a config
    // that adds one; once the node has carried it into the log, a config naming another is refused.
    let dir = scratch_dir("genesis-carried-notional");
    fs::create_dir_all(&dir).unwrap();
    crate::sequencer::fixture_tests::copy_fixture(V1_DEV, dir.join("00000000000000000000.jsonl"));
    let config = finished_genesis(&log_entries(&dir));
    let Some(Command::Admin(AdminCommand::SetParams(params))) = config.first() else {
        panic!("genesis sets parameters")
    };
    let weighting = params.toxicity.notional.expect("the config adds a weighting");
    recover_with(&dir, &signed(31_337), config.clone()).unwrap().carry_toxicity_notional(weighting).unwrap();
    recover_with(&dir, &signed(31_337), config.clone()).unwrap();

    let mut other = config;
    if let Some(Command::Admin(AdminCommand::SetParams(params))) = other.first_mut() {
        params.toxicity.notional = Some(NotionalWeighting { prior: weighting.memory, ..weighting });
    }
    let err = recover_with(&dir, &signed(31_337), other).err().unwrap();
    assert!(format!("{err:#}").contains("differs from the config's [genesis]"), "{err:#}");
}

#[test]
fn a_genesis_set_before_divergence_shares_resumes_under_the_shares_a_config_loads() {
    // A log from before `V13` set absolute divergence triggers, which no config loaded under the
    // latest protocol can give: its genesis compares as the shares `V13`'s activation turns them
    // into, and a config giving other shares is refused.
    let dir = scratch_dir("genesis-divergence-shares");
    fs::create_dir_all(&dir).unwrap();
    crate::sequencer::fixture_tests::copy_fixture(V1_DEV, dir.join("00000000000000000000.jsonl"));
    let logged = log_entries(&dir);
    let ran = commands(&logged)
        .into_iter()
        .find_map(|cmd| match cmd {
            Command::Admin(AdminCommand::SetParams(params)) => Some(params),
            _ => None,
        })
        .expect("genesis sets parameters");
    let err = ran.validate_for(ProtocolVersion::LATEST).unwrap_err();
    assert!(err.contains("divergence_share_bps"), "{err}");

    let config = finished_genesis(&logged);
    let Some(Command::Admin(AdminCommand::SetParams(params))) = config.first() else {
        panic!("genesis sets parameters")
    };
    params.validate_for(ProtocolVersion::LATEST).unwrap();
    // 15 and 20 bps of the genesis markets' widest bound, 25 bps.
    let shares: Vec<_> =
        params.stress_levels.iter().filter_map(|level| level.trigger.as_ref()?.divergence_share_bps).collect();
    assert_eq!(shares, [6_000, 8_000]);
    recover_with(&dir, &signed(31_337), config.clone()).unwrap();

    let mut other = config;
    if let Some(Command::Admin(AdminCommand::SetParams(params))) = other.first_mut() {
        let share = params
            .stress_levels
            .iter_mut()
            .find_map(|level| level.trigger.as_mut()?.divergence_share_bps.as_mut())
            .expect("a divergence share");
        *share += 1;
    }
    let err = recover_with(&dir, &signed(31_337), other).err().unwrap();
    assert!(format!("{err:#}").contains("differs from the config's [genesis]"), "{err:#}");
}

#[test]
fn a_genesis_no_current_config_can_name_is_refused_and_a_fresh_log_advised() {
    // An older protocol applied each genesis, and config load under the latest refuses it, so no
    // config can name it: recovery refuses whatever genesis the config gives, and writes nothing.
    // A log that ends at genesis can be removed; once a command follows genesis, the log may hold
    // client funds, so the advice is to move it aside, never to remove it.
    let logs = [
        ("genesis-only", Vec::new(), "remove node.log_dir", "history.dir aside"),
        ("followed", vec![deposit(100, "dep-1")], "move node.log_dir and history.dir aside", "remove node.log_dir"),
    ];
    for (shape, version, genesis, reason) in geneses_no_config_can_name() {
        for (log, after, advice, other_advice) in &logs {
            let dir = scratch_dir(&format!("genesis-unnameable-{shape}-{log}"));
            old_genesis_log(&dir, &signed(31_337), version, genesis.clone(), after.clone());
            let written = log_entries(&dir);
            for config in [finished_genesis(&written), genesis_commands(&dev_config().genesis)] {
                let err = format!("{:#}", recover_with(&dir, &signed(31_337), config).err().unwrap());
                assert!(err.contains("no current config can name this log's genesis"), "{shape}, {log}: {err}");
                assert!(err.contains(reason) && err.contains("a fresh log is needed"), "{shape}, {log}: {err}");
                assert!(err.contains(advice) && !err.contains(other_advice), "{shape}, {log}: {err}");
                assert!(!err.contains("started with"), "{shape}, {log}: {err}");
            }
            assert_eq!(log_entries(&dir), written, "{shape}, {log}: recovery wrote nothing");
        }
    }
}
