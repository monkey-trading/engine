//! Recovery, activation, and replay with the operator the log records, and with an operator the
//! registry installed in its place.

use super::*;

use engine_core::access::AccessPolicy;
use engine_core::benchmark::BenchmarkBatch;
use engine_core::eip712::{self, B256};
use engine_core::election::{RegistryElection, RegistryInstallation};
use engine_core::emissions::TokenUnits;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::staking::StakingConfig;
use engine_core::turns::{RegistryRotationSet, RegistryTurn, SetMember, TurnReason, boundary};
use k256::ecdsa::SigningKey;

use crate::admin::sign;
use crate::checkpoints::RegistryOperator;
use crate::trusted_time::tests::logs::Logs;

const START: Millis = 1_792_530_000_000;
/// The finalised block time the log's staking scan has reached.
const FRONTIER: Millis = START - 3_600_000;
/// The dev config's operator key, which the log records.
const RECORDED_SEED: u8 = 0x42;
/// The admin key the registry installed for the failed operator.
const INSTALLED_SEED: u8 = 0x55;

#[test]
fn a_v7_log_records_the_operator_and_activates_the_latest_version_at_its_next_seq() {
    let dir = scratch_dir("activation-v7");
    let client_auth = ClientAuth::Eip712 { chain_id: 31337, vault: AccountId([0; 20]) };
    let v7 = vec![
        Command::Admin(AdminCommand::SetClientAuth(client_auth.clone())),
        Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V3 }),
        Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V7 }),
    ];
    recover(&dir, &client_auth).sequence_required("V7 activation", v7).unwrap();
    recover(&dir, &client_auth).activate_protocol().unwrap();
    let written = log_entries(&dir);
    let cmds = commands(&written);
    assert_eq!(
        cmds[3..],
        [
            Command::Admin(AdminCommand::SetOperator(dev_config().operator().unwrap())),
            Command::Admin(AdminCommand::ActivateCustodyFinality {
                version: ProtocolVersion::LATEST,
                legacy_deposits: vec![],
            }),
        ]
    );
    let replayed = replay(&written).unwrap();
    assert_eq!(
        replayed.activations,
        [(ProtocolVersion::V3, 1), (ProtocolVersion::V7, 2), (ProtocolVersion::LATEST, 4)]
    );
}

#[test]
fn a_log_recording_another_operator_refuses_to_start_and_writes_nothing() {
    let dir = scratch_dir("activation-other-operator");
    recover(&dir, &signed(31_337)).activate_protocol().unwrap();
    let written = log_entries(&dir).len();
    let operator = dev_config().operator().unwrap();
    for other in [Operator { address: AccountId([9; 20]), ..operator }, Operator { chain_id: 1, ..operator }] {
        let (stream, _) = broadcast::channel(16);
        let (manifest, _) = watch::channel(Manifest::default());
        let cfg = SequencerConfig { client_auth: signed(31_337), operator: other, ..sequencer_config(&dir, manifest) };
        let recovered = Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), stream, mpsc::unbounded_channel().0);
        let err = recovered.err().unwrap();
        assert!(format!("{err:#}").contains("the log records operator"), "{err:#}");
    }
    assert_eq!(log_entries(&dir).len(), written);
}

#[test]
fn a_log_claiming_an_unsigned_operator_command_applied_after_funds_does_not_replay() {
    let dir = scratch_dir("unsigned-admin");
    let mut seq = recover(&dir, &ClientAuth::Unsigned);
    seq.activate_legacy_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.sequence(deposit(100, "dep-1")).unwrap();
    let every_market = Command::Admin(AdminCommand::SetReduceOnly { market: None, reduce_only: true });
    let events = seq.sequence(every_market.clone()).unwrap().expect("trusted time admits a stamp");
    assert!(matches!(&events[..], [Event::AdminRejected { .. }]), "{events:?}");
    seq.flush().unwrap();
    drop(seq);

    // As if the host rewrote the entry to say it applied.
    let mut entries = log_entries(&dir);
    let forged = entries.iter_mut().find_map(|entry| match entry {
        LogEntry::Command(c) if c.sequenced.cmd == every_market => Some(c),
        _ => None,
    });
    let forged = forged.expect("the command is logged");
    forged.events = Some(vec![Event::AdminApplied]);
    let err = replay(&entries).err().unwrap();
    assert!(format!("{err:#}").contains("invalid command origin signature"), "{err:#}");
    // Without its seal, as in a historical entry, replay still refuses the rewritten events.
    let unsealed = entries.iter_mut().find_map(|entry| match entry {
        LogEntry::Command(c) if c.sequenced.cmd == every_market => Some(c),
        _ => None,
    });
    unsealed.expect("the command is logged").seal = None;
    let err = replay(&entries).err().unwrap();
    assert!(format!("{err:#}").contains("differ from the events replay gives"), "{err:#}");
}

#[test]
fn recovery_admits_the_key_the_finalised_registry_installed_before_the_log_mirrors_it() {
    let (dir, clock) = staked_log("installed-admission", Some(FRONTIER));
    let written = log_entries(&dir).len();
    let recorded = dev_config().operator().unwrap();
    let key = address(INSTALLED_SEED);
    let installed = Operator { address: key, ..recorded };
    let seated = |admin_key, since| Some(RegistryOperator { admin_key, since });
    // Refused without the registry's word, when it names another key, when the log's scan has
    // passed the seating (so the log would hold its mirror), and on another deployment.
    for (operator, registry) in [
        (installed, None),
        (installed, seated(AccountId([0x66; 20]), FRONTIER)),
        (installed, seated(key, FRONTIER - 1)),
        (Operator { chain_id: recorded.chain_id + 1, ..installed }, seated(key, FRONTIER)),
    ] {
        let err = recover_installed(&dir, clock.clone(), operator, registry).err().expect("refused");
        assert!(format!("{err:#}").contains("the log records operator"), "{registry:?}: {err:#}");
    }
    // A seating in the frontier's own second may sit in a later block the scan has not read.
    let seq = recover_installed(&dir, clock.clone(), installed, seated(key, FRONTIER)).unwrap();
    assert_eq!(seq.engine.operator(), Some(&recorded), "the log keeps its key until the mirror's notice runs");
    drop(seq);
    assert_eq!(log_entries(&dir).len(), written);

    // A log whose scan records no frontier time cannot tell a seating it has not mirrored.
    let (dir, clock) = staked_log("installed-admission-unscanned", None);
    let err = recover_installed(&dir, clock, installed, seated(key, FRONTIER)).err().expect("refused");
    assert!(format!("{err:#}").contains("the log records operator"), "{err:#}");
}

#[test]
fn a_key_installed_again_is_a_new_seating() {
    // The log mirrors an installation, turning rotation on drops its handover, and with rotation off
    // again the timelock installs the same key, in a block before the lagging mirror logged the
    // first: the log's record of the first installation does not stand for the second.
    let (dir, clock) = staked_log("installed-again", Some(FRONTIER));
    let recorded = dev_config().operator().unwrap();
    let key = address(INSTALLED_SEED);
    let mut seq = recover_installed(&dir, clock.clone(), recorded, None).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let install = RegistryInstallation {
        candidate: AccountId([0xc2; 20]),
        admin_key: key,
        block: 11,
        chain_ref: "0x01:0".into(),
    };
    for cmd in [
        Command::InstallOperator(install),
        Command::SwitchRotation { enabled: true, election_cancelled: false, block: 12, chain_ref: "0x02:0".into() },
        Command::SwitchRotation { enabled: false, election_cancelled: false, block: 13, chain_ref: "0x03:0".into() },
    ] {
        let events = seq.sequence(cmd).unwrap().expect("admitted");
        assert!(!events.iter().any(|event| matches!(event, Event::ElectionRejected { .. })), "{events:?}");
    }
    assert_eq!(seq.engine.operator_bounds().rotation, None, "turning rotation on dropped the handover");
    let logged =
        seq.engine.operator_election().installed.expect("the installation is recorded").effective_at - NOTICE_MS;
    seq.flush().unwrap();
    drop(seq);

    let again = Some(RegistryOperator { admin_key: key, since: logged - 1_000 });
    let mut seq = start_over_a_rollover(&dir, &clock, Operator { address: key, ..recorded }, again);
    assert!(seq.sequence(Command::Tick).unwrap().is_some(), "no hold waits on the key the installation replaces");
}

#[test]
fn a_rotation_no_installation_made_keeps_the_hold() {
    // A signed rotation's notice, and an election's, leave the recorded key to sign: a hold waits.
    for elected in [false, true] {
        let (dir, clock) = staked_log(&format!("installed-hold-rotation-{elected}"), Some(FRONTIER));
        let recorded = dev_config().operator().unwrap();
        let next = address(INSTALLED_SEED);
        let mut seq = recover_installed(&dir, clock.clone(), recorded, None).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        let cmds = if elected {
            let election = RegistryElection {
                epoch: 1,
                candidate: AccountId([0xc1; 20]),
                admin_key: next,
                effective_at: clock.now_ms() + NOTICE_MS,
                block: 12,
                chain_ref: "0x0b:0".into(),
            };
            vec![
                Command::SwitchRotation {
                    enabled: true,
                    election_cancelled: false,
                    block: 11,
                    chain_ref: "0x0a:0".into(),
                },
                Command::ElectOperator(election),
            ]
        } else {
            vec![signed_by(&seq, &clock, RECORDED_SEED, AdminCommand::RotateOperator { address: next })]
        };
        for cmd in cmds {
            let events = seq.sequence(cmd).unwrap().expect("admitted");
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::ElectionRejected { .. } | Event::AdminRejected { .. })),
                "{events:?}"
            );
        }
        assert_eq!(seq.engine.operator_bounds().rotation.map(|rotation| rotation.address), Some(next));
        seq.flush().unwrap();
        drop(seq);
        let mut seq = start_over_a_rollover(&dir, &clock, recorded, None);
        assert!(seq.sequence(Command::Tick).unwrap().is_none(), "elected: {elected}");
    }
}

#[test]
fn a_due_key_an_unscanned_installation_supersedes_ends_no_hold() {
    // The log holds a first installation, due but its key gone too, and the registry has since
    // installed another: only the second installation's key could end a hold, and it is not due.
    let (dir, clock) = staked_log("installed-superseded-due", Some(FRONTIER));
    let recorded = dev_config().operator().unwrap();
    let (first, second) = (address(INSTALLED_SEED), AccountId([0x77; 20]));
    let mut seq = recover_installed(&dir, clock.clone(), recorded, None).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let install = RegistryInstallation {
        candidate: AccountId([0xc2; 20]),
        admin_key: first,
        block: 11,
        chain_ref: "0x01:0".into(),
    };
    let events = seq.sequence(Command::InstallOperator(install)).unwrap().expect("admitted");
    assert!(events.iter().any(|event| matches!(event, Event::OperatorInstalled { .. })), "{events:?}");
    seq.flush().unwrap();
    drop(seq);

    clock.advance(Duration::from_millis(NOTICE_MS as u64 + 1_000));
    let registry = Some(RegistryOperator { admin_key: second, since: clock.now_ms() - 60_000 });
    let mut seq = recover_installed(&dir, clock.clone(), Operator { address: second, ..recorded }, registry).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    assert_eq!(seq.engine.due_rotation(clock.now_ms()).map(|rotation| rotation.address), Some(first));
    seq.wait_for_benchmarks().unwrap();
    assert!(seq.sequence(Command::Tick).unwrap().is_some(), "the due key is superseded");
}

#[test]
fn a_turns_member_takes_it_with_its_first_command_once() {
    let (dir, clock, turn, member) = turn_log("turn-taken");
    let release = B256::repeat_byte(0x51);
    let recorded = dev_config().operator().unwrap();
    let mut seq = recover_turns(&dir, clock.clone(), Operator { address: member, ..recorded }, release);
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    seq.flush().unwrap();
    let written = commands(&log_entries(&dir));
    let taken: Vec<_> = written.iter().filter(|cmd| matches!(cmd, Command::TurnTaken { .. })).collect();
    assert_eq!(taken, [&Command::TurnTaken { turn, release }], "one TurnTaken");
    let at = written.iter().position(|cmd| matches!(cmd, Command::TurnTaken { .. })).unwrap();
    assert!(matches!(written[at - 1], Command::AssignTurn(_)), "the turn's first command takes it");
    assert_eq!(seq.engine.daily_turns().taken.map(|taken| (taken.turn, taken.release)), Some((turn, release)));
}

#[test]
fn a_turns_member_takes_it_behind_a_benchmark_hold() {
    let (dir, clock, turn, member) = turn_log("turn-taken-held");
    let (release, recorded) = (B256::repeat_byte(0x53), dev_config().operator().unwrap());
    let mut seq = recover_turns(&dir, clock.clone(), Operator { address: member, ..recorded }, release);
    assert!(seq.engine.next_rollover().is_some_and(|next| next <= clock.now_ms()), "the day's rollover is unpaid");
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.wait_for_benchmarks().unwrap();
    assert!(seq.sequence(Command::Tick).unwrap().is_none(), "the hold admits no tick");
    seq.flush().unwrap();
    let written = commands(&log_entries(&dir));
    assert_eq!(written.last(), Some(&Command::TurnTaken { turn, release }), "the turn is taken behind the hold");
}

#[test]
fn a_turns_member_holds_nothing_at_its_start_once_the_incumbent_paid_the_rollover() {
    // The incumbent's last stamp passed the day's rollover, as its feeds' prices do before a boundary.
    let (dir, clock, turn, member) = turn_log_under("turn-taken-current", &[], None);
    let (begins, recorded) = (boundary(turn).unwrap(), dev_config().operator().unwrap());
    clock.advance(Duration::from_millis((begins - 60_000 - clock.now_ms()) as u64));
    let mut incumbent = recover_installed(&dir, clock.clone(), recorded, None).unwrap();
    incumbent.trust(sample_around(clock.as_ref())).unwrap();
    incumbent.sequence(Command::Tick).unwrap().expect("admitted");
    incumbent.flush().unwrap();
    drop(incumbent);
    clock.advance(Duration::from_millis(61_000));

    let release = B256::repeat_byte(0x57);
    let mut seq = recover_turns(&dir, clock.clone(), Operator { address: member, ..recorded }, release);
    assert!(seq.engine.next_rollover().is_some_and(|next| next > clock.now_ms()), "no rollover is unpaid");
    seq.wait_for_benchmarks().unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    assert!(seq.sequence(Command::Tick).unwrap().is_some(), "the member's start holds nothing");
    assert!(!seq.benchmark_sync);
    seq.flush().unwrap();
    let written = commands(&log_entries(&dir));
    assert_eq!(written[written.len() - 2..], [Command::TurnTaken { turn, release }, Command::Tick]);
}

#[test]
fn a_node_under_another_key_takes_no_turn() {
    let (dir, clock, _, _) = turn_log("turn-not-taken");
    let mut seq = recover_turns(&dir, clock.clone(), dev_config().operator().unwrap(), B256::repeat_byte(0x52));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    assert_eq!(seq.engine.daily_turns().taken, None);
    assert!(!commands(&log_entries(&dir)).iter().any(|cmd| matches!(cmd, Command::TurnTaken { .. })));
}

/// The address of `seed`'s signing key.
fn address(seed: u8) -> AccountId {
    eip712::address(SigningKey::from_slice(&[seed; 32]).unwrap().verifying_key())
}

/// A benchmark snapshot for today's date that checks every listed benchmark and finds no rate,
/// signed by `seed`'s key.
fn snapshot(seq: &Sequencer, clock: &TestClock, seed: u8) -> Command {
    let batch = BenchmarkBatch {
        as_of: seq.engine.benchmark_date(clock.now_ms()),
        checked: seq.engine.benchmark_state().required,
        rates: Vec::new(),
    };
    signed_by(seq, clock, seed, AdminCommand::SetBenchmarkRates(batch))
}

/// `command` as `seed`'s key signs it, at its next nonce.
fn signed_by(seq: &Sequencer, clock: &TestClock, seed: u8, command: AdminCommand) -> Command {
    let key = SigningKey::from_slice(&[seed; 32]).unwrap();
    let nonce = seq.engine.next_request_nonce(address(seed));
    Command::SignedAdmin(Box::new(sign(&key, &seq.operator, command, nonce, clock.now_ms() + 60_000)))
}

/// A log in a fresh scratch directory whose operator is the config's, with staking initialised and
/// its staking scan's frontier at `frontier`, or none completed, and the clock its stamps run on.
fn staked_log(name: &str, frontier: Option<Millis>) -> (PathBuf, Arc<TestClock>) {
    staked_log_under(name, frontier, None)
}

/// `staked_log`, recording the access policy `policy`, if any.
fn staked_log_under(name: &str, frontier: Option<Millis>, policy: Option<AccessPolicy>) -> (PathBuf, Arc<TestClock>) {
    staked_log_with(name, frontier, policy, |_| {})
}

/// `staked_log_under`, with the house starting with no capital.
fn staked_log_without_capital(
    name: &str,
    frontier: Option<Millis>,
    policy: Option<AccessPolicy>,
) -> (PathBuf, Arc<TestClock>) {
    staked_log_with(name, frontier, policy, |genesis| genesis.house_capital = Fixed::ZERO)
}

/// `staked_log_under`, with `change` applied to the dev genesis.
fn staked_log_with(
    name: &str,
    frontier: Option<Millis>,
    policy: Option<AccessPolicy>,
    change: impl FnOnce(&mut crate::config::Genesis),
) -> (PathBuf, Arc<TestClock>) {
    let dir = scratch_dir(name);
    let clock = Arc::new(TestClock::at(START));
    let recorded = dev_config().operator().unwrap();
    let mut seq = recover_installed_under(&dir, clock.clone(), recorded, None, policy).unwrap();
    seq.activate_protocol().unwrap();
    let mut genesis = dev_config().genesis;
    genesis.benchmarks.clear();
    change(&mut genesis);
    genesis.staking = Some(StakingConfig {
        liquidity_floor: Fixed::from_int(1),
        epoch_ms: 86_400_000,
        start_ms: START + 3_600_000,
        cooldown_ms: 86_400_000,
    });
    seq.genesis(&genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    if let Some(time) = frontier {
        let progress = Command::StakingProgress {
            block: 10,
            block_hash: B256::repeat_byte(1),
            block_time: Some(time),
            named_turn: None,
        };
        let events = seq.sequence(progress).unwrap().expect("admitted");
        assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    }
    seq.flush().unwrap();
    (dir, clock)
}

/// Recovers the log in `dir` under a config naming `operator`, with the finalised registry naming
/// `registry` in effect.
/// Error cases:
/// - recovery fails (see `Sequencer::recover`)
fn recover_installed(
    dir: &Path,
    clock: Arc<TestClock>,
    operator: Operator,
    registry: Option<RegistryOperator>,
) -> anyhow::Result<Sequencer> {
    recover_installed_under(dir, clock, operator, registry, None)
}

/// `recover_installed`, under the access policy `access_policy`, if any.
/// Error cases:
/// - recovery fails (see `Sequencer::recover`)
fn recover_installed_under(
    dir: &Path,
    clock: Arc<TestClock>,
    operator: Operator,
    registry: Option<RegistryOperator>,
    access_policy: Option<AccessPolicy>,
) -> anyhow::Result<Sequencer> {
    let genesis = finished_genesis(&log_entries(dir));
    let (manifest, _) = watch::channel(Manifest::default());
    let registry_operator = registry;
    let base = sequencer_config(dir, manifest);
    let cfg = SequencerConfig { clock, genesis, operator, registry_operator, access_policy, ..base };
    Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(16).0, mpsc::unbounded_channel().0)
}

/// `recover_installed` as a node with `[benchmarks]` starts, at the log's next rollover or later, so its
/// first stamp finds a rollover unpaid and holds for a snapshot unless no key can sign one.
fn start_over_a_rollover(
    dir: &Path,
    clock: &Arc<TestClock>,
    operator: Operator,
    registry: Option<RegistryOperator>,
) -> Sequencer {
    let mut seq = recover_installed(dir, clock.clone(), operator, registry).unwrap();
    let next = seq.engine.next_rollover().expect("a rollover is scheduled");
    clock.advance(Duration::from_millis(u64::try_from(next - clock.now_ms()).unwrap_or(0)));
    seq.wait_for_benchmarks().unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq
}

mod faults;
mod governance;
mod hold;
mod turn_log;
pub(crate) use turn_log::{recover_turns, turn_log, turn_log_under};
