//! Test-only adaptation of immutable protocol fixtures whose signatures are placeholders.

use std::path::Path;
use std::sync::Arc;

use engine_core::Engine;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command};
use engine_core::protocol::ProtocolVersion;
use tokio::sync::{broadcast, mpsc, watch};

use crate::log::{CheckpointDomain, LogEntry, Manifest, read_file};
use crate::sequencer::genesis::{GENESIS_REF, rejection};
use crate::sequencer::{Sequencer, SequencerConfig};
use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, sequencer_config, trusted_now};

/// Signs fixture roots for tests of historical protocol replay; never used by node recovery.
pub(crate) fn signed_fixture(path: &Path) -> Vec<LogEntry> {
    let key = EnclaveKey::test_key();
    let cfg = crate::test_support::dev_config();
    let mut engine = Engine::new();
    let mut entries = Vec::new();
    read_file(path, |mut entry| {
        match &mut entry {
            LogEntry::Command(command) => {
                engine.apply(&command.sequenced)?;
            }
            LogEntry::Checkpoint(cp) => {
                let domain = engine
                    .operator()
                    .map(|operator| CheckpointDomain { chain_id: operator.chain_id, vault: operator.vault })
                    .or_else(|| match engine.client_auth() {
                        Some(ClientAuth::Eip712 { chain_id, vault }) => {
                            Some(CheckpointDomain { chain_id: *chain_id, vault: *vault })
                        }
                        _ => None,
                    })
                    .unwrap_or(CheckpointDomain {
                        chain_id: cfg.custody.chain_id,
                        vault: cfg.custody.vault.parse().unwrap(),
                    });
                cp.signature = key.sign_checkpoint(
                    domain.chain_id,
                    &domain.vault.0,
                    cp.seq,
                    &cp.root.parse::<engine_core::eip712::B256>().unwrap().0,
                );
                cp.signer = key.address_hex();
                cp.domain = Some(domain);
            }
            LogEntry::Withdrawal(w) => {
                let domain = engine
                    .operator()
                    .map(|operator| CheckpointDomain { chain_id: operator.chain_id, vault: operator.vault })
                    .or_else(|| match engine.client_auth() {
                        Some(ClientAuth::Eip712 { chain_id, vault }) => {
                            Some(CheckpointDomain { chain_id: *chain_id, vault: *vault })
                        }
                        _ => None,
                    })
                    .unwrap_or(CheckpointDomain {
                        chain_id: cfg.custody.chain_id,
                        vault: cfg.custody.vault.parse().unwrap(),
                    });
                w.chain_id = domain.chain_id;
                w.vault = domain.vault;
                w.signature = key.sign_withdrawal(w.chain_id, &w.vault.0, w.account, w.amount, w.nonce);
            }
        }
        entries.push(entry);
        Ok(())
    })
    .unwrap();
    entries
}

/// Writes a signed fixture copy, leaving the committed input untouched.
pub(crate) fn copy_fixture(source: &str, target: impl AsRef<Path>) {
    let text: String = signed_fixture(source.as_ref())
        .iter()
        .map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap()))
        .collect();
    std::fs::write(target, text).unwrap();
}

/// The genesis `entries` ran, as the config of a dev node that resumes the log gives it.
pub(crate) fn finished_genesis(entries: &[LogEntry]) -> Vec<engine_core::command::Command> {
    let notional = crate::test_support::dev_config().genesis.params.toxicity.notional;
    replay_trusted(entries).expect("the log replays").genesis.as_config(notional)
}

/// Replays harness output against the explicit fixture authority, never a log-named signer.
pub fn replay_trusted(entries: &[LogEntry]) -> anyhow::Result<crate::sequencer::Replayed> {
    let domain = entries
        .iter()
        .find_map(|entry| if let LogEntry::Checkpoint(cp) = entry { cp.domain } else { None })
        .unwrap_or(crate::log::CheckpointDomain { chain_id: 31337, vault: engine_core::types::AccountId([0; 20]) });
    crate::sequencer::replay_with_signer(
        entries,
        domain,
        engine_core::types::AccountId(EnclaveKey::test_key().address()),
    )
}

/// Genesis shapes an older protocol applied and config load under the latest refuses, each named, with
/// the version that applied it and the reason config load gives: a stress and toxicity spread that
/// reaches the price before `V8`, a divergence trigger beside a zero-bound market before `V13`, and a
/// market on a benchmark genesis gave no rate before `V14`.
pub(crate) fn geneses_no_config_can_name() -> Vec<(&'static str, ProtocolVersion, Vec<Command>, &'static str)> {
    let dev = dev_config().genesis;
    // The dev parameters without divergence triggers, a form every version takes.
    let mut params = dev.params.clone();
    params.stress_levels.iter_mut().filter_map(|level| level.trigger.as_mut()).for_each(|trigger| {
        trigger.divergence_share_bps = None;
    });
    let (btc, eth) = (dev.markets[0].clone(), dev.markets[1].clone());
    let genesis = |params, second| {
        vec![
            Command::Admin(AdminCommand::SetParams(params)),
            Command::Admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 400 }),
            Command::Admin(AdminCommand::UpsertMarket(btc.clone())),
            Command::Admin(AdminCommand::UpsertMarket(second)),
            Command::HouseCapital { amount: dev.house_capital, chain_ref: GENESIS_REF.into() },
        ]
    };
    // With the 3 bps toxicity tier, 10_001 bps.
    let mut spread = params.clone();
    spread.stress_levels.last_mut().expect("stress levels").min_spread_bps = 9_998;
    let mut divergence = params.clone();
    divergence.stress_levels[1].trigger.as_mut().expect("a level 1 trigger").divergence_bps = Some(15);
    let mut unbounded = eth.clone();
    unbounded.price_rules.max_divergence_bps = 0;
    let mut rateless = eth.clone();
    rateless.financing.benchmark = "ESTR".into();
    vec![
        ("closing-spread", ProtocolVersion::V7, genesis(spread, eth), "no closing trade could fill"),
        ("zero-bound", ProtocolVersion::V12, genesis(divergence, unbounded), "can never fire in ETH-USD"),
        ("rateless-benchmark", ProtocolVersion::V13, genesis(params, rateless), "benchmark ESTR has no rate"),
    ]
}

/// Writes to `dir` a log that recorded `client_auth` and the dev operator, activated `version`, ran
/// `genesis`, and closed it with a checkpoint, then sequenced `after`, if any, and closed it with another,
/// as a node of that version did. Panics if that version rejects a command.
pub(crate) fn old_genesis_log(
    dir: &Path,
    client_auth: &ClientAuth,
    version: ProtocolVersion,
    genesis: Vec<Command>,
    after: Vec<Command>,
) {
    let manifest = watch::channel(Manifest::default()).0;
    let cfg =
        SequencerConfig { client_auth: client_auth.clone(), genesis: Vec::new(), ..sequencer_config(dir, manifest) };
    let key = Arc::new(EnclaveKey::test_key());
    let mut seq = Sequencer::recover(cfg, key, broadcast::channel(16).0, mpsc::unbounded_channel().0).unwrap();
    let ts = trusted_now().earliest_ms;
    let setup = [
        AdminCommand::SetClientAuth(client_auth.clone()),
        AdminCommand::SetOperator(seq.operator),
        AdminCommand::ActivateProtocol { version },
    ];
    let opening = setup.map(Command::Admin).into_iter().chain(genesis).collect();
    for commands in [opening, after].into_iter().filter(|commands: &Vec<Command>| !commands.is_empty()) {
        for cmd in commands {
            let events = seq.stamp(ts, cmd).unwrap();
            assert_eq!(rejection(&events), None, "{version:?} applies every command");
        }
        seq.checkpoint().unwrap();
    }
    seq.flush().unwrap();
}
