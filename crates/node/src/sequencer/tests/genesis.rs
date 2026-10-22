//! Genesis is all or nothing: a genesis the engine would reject writes nothing, and recovery refuses
//! a log whose genesis did not reach the checkpoint after its house capital credit, or is not the
//! config's.

use super::*;

use engine_core::params::NotionalWeighting;

use crate::config::{CustodyProvider, Genesis, NodeConfig};
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
    let cases: [(&str, Edit); 6] = [
        ("genesis.params: the engine rejects it: profit_fee_bps", |g| g.params.profit_fee_bps = 10_000),
        ("genesis benchmark SONIA: the engine rejects it", |g| g.benchmarks[1].annual_bps = i64::MIN),
        ("genesis market EUR-USD: the engine rejects it: margin_tiers is empty", |g| g.markets[2].margin_tiers.clear()),
        ("genesis market AAPL: the engine rejects it: house_cap_bps", |g| {
            g.markets.last_mut().unwrap().house_cap_bps = 0
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
        let activation = |cmd: &Command| {
            matches!(
                cmd,
                Command::Admin(
                    AdminCommand::SetClientAuth(_)
                        | AdminCommand::SetOperator(_)
                        | AdminCommand::ActivateProtocol { .. }
                )
            )
        };
        assert!(!written.is_empty() && written.iter().all(activation), "{expected}: {written:?}");
        assert!(seq.needs_genesis(), "{expected}");
    }
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
            seq.activate_protocol().unwrap();
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
    fs::copy(V1_DEV, dir.join("00000000000000000000.jsonl")).unwrap();
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
