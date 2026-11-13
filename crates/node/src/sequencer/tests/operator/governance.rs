//! Parameter votes from `V56`: the config's vote contract recorded once, a log recording another
//! refused, a passed change applied at the first stamp past its notice, and the node's own access
//! policy waiting for a vote while rotation is on.

use super::*;

use engine_core::access::AccessPolicy;
use engine_core::governance::{GovernanceConfig, PassedProposal, command_hash};
use engine_core::market::{MarketSpec, Tier};

const VOTE: AccountId = AccountId([0x5f; 20]);

fn config(ttl_ms: Millis) -> GovernanceConfig {
    GovernanceConfig { vote: VOTE, restriction_ttl_ms: ttl_ms }
}

#[test]
fn the_config_vote_contract_is_recorded_once_and_another_is_refused() {
    let (dir, clock) = staked_log("vote-recorded", Some(FRONTIER));
    let activate = |governance| {
        let mut seq = recover_governed(&dir, clock.clone(), governance, None)?;
        seq.activate_protocol()?;
        seq.trust(sample_around(clock.as_ref()))
    };
    activate(Some(config(3_600_000))).unwrap();
    let recorded = commands(&log_entries(&dir));
    assert_eq!(recorded.last(), Some(&Command::Admin(AdminCommand::SetGovernance(config(3_600_000)))));
    activate(Some(config(3_600_000))).unwrap();
    assert_eq!(commands(&log_entries(&dir)).len(), recorded.len(), "recorded once");
    // Recovery refuses another config before anything is written.
    for other in [Some(config(7_200_000)), None] {
        let error = activate(other).unwrap_err();
        assert!(format!("{error:#}").contains("the log records vote contract"), "{error:#}");
    }
    assert_eq!(commands(&log_entries(&dir)).len(), recorded.len(), "nothing written");
}

#[test]
fn a_passed_change_applies_at_the_first_stamp_past_its_notice() {
    let (dir, clock) = staked_log("vote-applied", Some(FRONTIER));
    let mut seq = recover_governed(&dir, clock.clone(), Some(config(3_600_000)), None).unwrap();
    seq.activate_protocol().unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    rotation_on(&mut seq);
    let (applies_at, margin_tiers) = pass_raise(&mut seq);
    // `GET /v1/house/state` lists the change waiting, with the time it applies from.
    let state = serde_json::to_value(house_state(&mut seq)).unwrap();
    assert_eq!(state["governance"]["pending"][0]["proposal"], serde_json::json!(7), "{state}");
    assert_eq!(state["governance"]["pending"][0]["applies_at"], serde_json::json!(applies_at), "{state}");
    assert_eq!(state["governance"]["config"]["vote"], serde_json::json!(VOTE), "{state}");
    clock.advance(Duration::from_millis((applies_at - 1 - clock.now_ms()) as u64));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    assert!(!commands(&log_entries_flushed(&mut seq, &dir)).iter().any(|cmd| matches!(cmd, Command::ApplyProposal(_))));
    clock.advance(Duration::from_millis(1));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    let written = commands(&log_entries_flushed(&mut seq, &dir));
    let applied = written.iter().position(|cmd| matches!(cmd, Command::ApplyProposal(voted) if voted.proposal == 7));
    let applied = applied.expect("the change is sequenced");
    assert_eq!(written[applied + 1..], [Command::Tick], "ahead of the command stamped then, once");
    assert_eq!(seq.engine.markets()[&1].spec.margin_tiers, margin_tiers);
    assert!(seq.engine.governance().pending.is_empty());
    assert_eq!(serde_json::to_value(house_state(&mut seq)).unwrap()["governance"]["pending"], serde_json::json!(null));
}

/// What the sequencer answers `GET /v1/house/state` with now, once the batch is flushed.
fn house_state(seq: &mut Sequencer) -> crate::sequencer::house::HouseState {
    let (reply, mut answer) = oneshot::channel();
    seq.house.wait(reply);
    seq.flush().unwrap();
    answer.try_recv().expect("a flush answers the waiting read")
}

#[test]
fn while_rotation_is_on_the_access_policy_waits_for_a_vote() {
    let (dir, clock) = staked_log("vote-access", Some(FRONTIER));
    let mut seq = recover_governed(&dir, clock.clone(), Some(config(3_600_000)), None).unwrap();
    seq.activate_protocol().unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    rotation_on(&mut seq);
    seq.flush().unwrap();
    drop(seq);
    let policy = AccessPolicy { source: AccountId([0x5a; 20]), max_age_ms: 60_000 };
    let before = commands(&log_entries(&dir)).len();
    let mut seq = recover_governed(&dir, clock.clone(), Some(config(3_600_000)), Some(policy)).unwrap();
    seq.activate_protocol().unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    assert_eq!(commands(&log_entries(&dir)).len(), before, "the node sequences no access policy");
    assert!(seq.engine.access_policy().is_none());
}

#[test]
fn a_passed_change_due_behind_a_benchmark_hold_waits_for_the_snapshot() {
    let (dir, clock) = staked_log("vote-benchmark-hold", Some(FRONTIER));
    let mut seq = recover_governed(&dir, clock.clone(), Some(config(3_600_000)), None).unwrap();
    seq.activate_protocol().unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    rotation_on(&mut seq);
    let (applies_at, margin_tiers) = pass_raise(&mut seq);
    clock.advance(Duration::from_millis((applies_at - clock.now_ms()) as u64));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.wait_for_benchmarks().unwrap();
    let snapshot = snapshot(&seq, &clock, RECORDED_SEED);
    let events = seq.sequence(snapshot).unwrap().expect("the snapshot is admitted behind the hold");
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert!(!commands(&log_entries_flushed(&mut seq, &dir)).iter().any(|cmd| matches!(cmd, Command::ApplyProposal(_))));
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    let written = commands(&log_entries_flushed(&mut seq, &dir));
    let at = |pick: fn(&Command) -> bool| written.iter().position(pick).expect("logged");
    assert!(at(|cmd| cmd.is_benchmark_batch()) < at(|cmd| matches!(cmd, Command::ApplyProposal(_))));
    assert_eq!(seq.engine.markets()[&1].spec.margin_tiers, margin_tiers);
}

#[test]
fn voted_relistings_that_retire_a_benchmark_end_its_alerts() {
    let (dir, clock) = staked_log("vote-benchmark-retired", Some(FRONTIER));
    let mut seq = recover_governed(&dir, clock.clone(), Some(config(3_600_000)), None).unwrap();
    seq.activate_protocol().unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    rotation_on(&mut seq);
    // EUR-USD and Germany 40 are the markets on ESTR: both move to SOFR by vote.
    let mut applies_at = 0;
    for (proposal, market) in [(8, 10), (9, 23)] {
        let mut spec = seq.engine.baseline_spec(market).expect("listed");
        assert_eq!(spec.financing.benchmark, "ESTR");
        spec.financing.benchmark = "SOFR".into();
        let relist = AdminCommand::UpsertMarket(spec);
        let chain_ref = format!("0x{proposal:02x}:0");
        let passed = PassedProposal {
            proposal,
            fast: false,
            command_hash: command_hash(&relist),
            command: Some(relist),
            block: 12 + proposal,
            chain_ref,
        };
        let events = seq.sequence(Command::PassProposal(Box::new(passed))).unwrap().expect("admitted");
        let Some(Event::ProposalScheduled { applies_at: at, .. }) = events.last().cloned() else {
            panic!("{events:?}")
        };
        applies_at = at;
    }
    clock.advance(Duration::from_millis((applies_at - clock.now_ms()) as u64));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let (logs, _guard) = Logs::capture();
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    assert!(seq.engine.markets().values().all(|market| market.spec.financing.benchmark != "ESTR"));
    assert_eq!(
        logs.count("alert_resolved=\"benchmark_rate_missing\" benchmark=\"ESTR\""),
        1,
        "the retired benchmark's episode ends"
    );
}

#[test]
fn a_start_activating_v56_with_rotation_on_leaves_the_access_policy_to_a_vote() {
    let dir = scratch_dir("vote-activation-v55");
    let clock = Arc::new(TestClock::at(START));
    let mut seq = recover_governed(&dir, clock.clone(), None, None).unwrap();
    seq.activate_protocol_at(ProtocolVersion::V55).unwrap();
    let mut genesis = dev_config().genesis;
    genesis.benchmarks.clear();
    genesis.staking = Some(StakingConfig {
        liquidity_floor: Fixed::from_int(1),
        epoch_ms: 86_400_000,
        start_ms: START + 3_600_000,
        cooldown_ms: 86_400_000,
    });
    seq.genesis(&genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    rotation_on(&mut seq);
    seq.flush().unwrap();
    drop(seq);
    // The upgraded release's config adds an access policy, which V56 leaves to a vote while rotation is on.
    let policy = AccessPolicy { source: AccountId([0x5a; 20]), max_age_ms: 60_000 };
    let mut seq = recover_governed(&dir, clock.clone(), Some(config(3_600_000)), Some(policy)).unwrap();
    seq.activate_protocol().unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    assert_eq!(seq.engine.protocol_version(), ProtocolVersion::LATEST);
    assert_eq!(seq.engine.governance().config, Some(config(3_600_000)));
    assert!(seq.engine.access_policy().is_none());
}

#[test]
fn a_voted_ceiling_adoption_waiting_for_its_time_is_listed_with_it() {
    let (dir, clock) = staked_log("vote-ceiling", Some(FRONTIER));
    let mut seq = recover_governed(&dir, clock.clone(), Some(config(3_600_000)), None).unwrap();
    seq.activate_protocol().unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    rotation_on(&mut seq);
    let lower = seq.engine.close_spread_ceilings().expect("genesis adopts ceilings")[&1] - 1;
    let effective_at = clock.now_ms() + 30 * 86_400_000;
    let adoption =
        AdminCommand::AdoptCloseSpreadCeiling { market: 1, max_close_spread_bps: lower, effective_at, listing: None };
    let passed = PassedProposal {
        proposal: 11,
        fast: false,
        command_hash: command_hash(&adoption),
        command: Some(adoption),
        block: 12,
        chain_ref: "0x0b:0".into(),
    };
    let events = seq.sequence(Command::PassProposal(Box::new(passed))).unwrap().expect("admitted");
    let Some(Event::ProposalScheduled { applies_at, .. }) = events.last().cloned() else { panic!("{events:?}") };
    clock.advance(Duration::from_millis((applies_at - clock.now_ms()) as u64));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    let state = serde_json::to_value(house_state(&mut seq)).unwrap();
    assert_eq!(state["governance"]["pending"], serde_json::json!(null), "{state}");
    let waiting = serde_json::json!({ "market": 1, "max_close_spread_bps": lower, "effective_at": effective_at });
    assert_eq!(state["pending_ceiling"], waiting, "{state}");
}

/// Mirrors a passed normal proposal doubling BTC's first margin band: when it applies and the bands.
fn pass_raise(seq: &mut Sequencer) -> (Millis, Vec<Tier>) {
    let spec = seq.engine.baseline_spec(1).expect("BTC is listed");
    let mut margin_tiers = spec.margin_tiers.clone();
    margin_tiers[0] = Tier { bps: margin_tiers[0].bps * 2, ..margin_tiers[0] };
    let raise = AdminCommand::UpsertMarket(MarketSpec { margin_tiers: margin_tiers.clone(), ..spec });
    let passed = PassedProposal {
        proposal: 7,
        fast: false,
        command_hash: command_hash(&raise),
        command: Some(raise),
        block: 12,
        chain_ref: "0x07:0".into(),
    };
    let events = seq.sequence(Command::PassProposal(Box::new(passed))).unwrap().expect("admitted");
    let Some(Event::ProposalScheduled { applies_at, .. }) = events.last().cloned() else { panic!("{events:?}") };
    (applies_at, margin_tiers)
}

/// Turns rotation on in the log.
fn rotation_on(seq: &mut Sequencer) {
    let switch =
        Command::SwitchRotation { enabled: true, election_cancelled: false, block: 11, chain_ref: "0x01:0".into() };
    let events = seq.sequence(switch).unwrap().expect("admitted");
    assert!(matches!(events.last(), Some(Event::RotationSwitched { .. })), "{events:?}");
}

/// The log's entries once `seq` has flushed what it sequenced.
fn log_entries_flushed(seq: &mut Sequencer, dir: &Path) -> Vec<crate::log::LogEntry> {
    seq.flush().unwrap();
    log_entries(dir)
}

/// Recovers the log in `dir` under the dev config's operator, with the vote contract `governance` and
/// the access policy `access_policy`.
/// Error cases:
/// - recovery fails (see `Sequencer::recover`)
fn recover_governed(
    dir: &Path,
    clock: Arc<TestClock>,
    governance: Option<GovernanceConfig>,
    access_policy: Option<AccessPolicy>,
) -> anyhow::Result<Sequencer> {
    let genesis = finished_genesis(&log_entries(dir));
    let (manifest, _) = watch::channel(Manifest::default());
    let operator = dev_config().operator().unwrap();
    let base = sequencer_config(dir, manifest);
    let cfg = SequencerConfig { clock, genesis, operator, governance, access_policy, ..base };
    Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(16).0, mpsc::unbounded_channel().0)
}
