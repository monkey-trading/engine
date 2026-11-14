//! The API hostname the registry records for the operator in effect, read at a finalized block on
//! a bounded interval for `GET /v1/operator`: stream clients reconnect there after a handover. Under
//! daily turns it is the hostname of the turn in effect at trusted time, from the finalised schedule:
//! for the finality lag after each boundary the finalized registry still names the previous holder,
//! which only follows. The value comes only from the registry, never from this node's own config.

use std::time::Duration;

use alloy::primitives::B256;
use anyhow::{Context, ensure};
use engine_core::types::Millis;
use serde::Serialize;
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, info, warn};

use crate::checkpoints::{CheckpointChain, View};
use crate::config::NodeConfig;

/// A registered hostname and the finalized block it was read at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RegisteredHostname {
    pub hostname: String,
    pub block: u64,
    pub block_hash: B256,
}

/// The newest read, `None` while none has succeeded or the last one failed.
pub(crate) type HostnameFeed = watch::Receiver<Option<RegisteredHostname>>;

/// One read at the chain's finalized block: the hostname of the turn in effect at `reached`, trusted
/// time, if the finalised schedule names one, or else of the operator the registry names.
/// Error cases:
/// - the chain cannot be read, reads no registry, or pins no finalized block
pub(crate) async fn read(vault: &impl CheckpointChain, reached: Option<Millis>) -> anyhow::Result<RegisteredHostname> {
    let at = vault.observe(View::Finalized, &[]).await?;
    let evidence = at.lifecycle.as_ref().context("the finalized observation names no block")?;
    let hostname = match crate::handover::turns::turn_at(vault, &at, reached).await? {
        Some(turn) => vault.candidate_hostname_at(&at, turn.candidate).await?,
        None => vault.operator_hostname_at(&at).await?,
    };
    ensure!(!hostname.is_empty(), "the registry records no API hostname for the operator in effect");
    Ok(RegisteredHostname { hostname, block: evidence.height, block_hash: evidence.block_hash })
}

/// Reads every `every`, at the trusted time `reached` gives, and publishes the result on `feed`. A
/// failed read withdraws the value and raises Warning `operator_hostname_read_failed` until a read
/// succeeds.
/// Error cases:
/// - none; it runs until the node stops
pub(crate) async fn watch_hostname(
    vault: impl CheckpointChain,
    every: Duration,
    feed: watch::Sender<Option<RegisteredHostname>>,
    reached: impl Fn() -> Option<Millis>,
) -> anyhow::Result<()> {
    let mut ticks = interval(every);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut failed = false;
    loop {
        ticks.tick().await;
        match read(&vault, reached()).await {
            Ok(registered) => {
                debug!(block = registered.block, "operator hostname read at a finalized block");
                if std::mem::take(&mut failed) {
                    info!(alert_resolved = "operator_hostname_read_failed", "the operator's API hostname reads again");
                }
                feed.send_replace(Some(registered));
            }
            Err(cause) => {
                failed = true;
                feed.send_replace(None);
                warn!(
                    alert = "operator_hostname_read_failed",
                    "the registry's API hostname for the operator in effect cannot be read at a finalized block; \
                     /v1/operator omits it: {cause:#}"
                );
            }
        }
    }
}

/// While rotation is on, the certificate the enclave orders names the API hostname the registry
/// records for the operator in effect, or for `candidate`, the member whose turn this node takes, at
/// the finalized block: a mismatch would certify a hostname the candidate did not register, and
/// clients of the registered one would find no attested endpoint. Without `[api.tls.acme]` there is
/// no certificate to check.
/// Error cases:
/// - the chain cannot be read, or the registered hostname differs from the config's
pub(crate) async fn check_operator_hostname(
    cfg: &NodeConfig,
    vault: &impl CheckpointChain,
    candidate: Option<engine_core::types::AccountId>,
) -> anyhow::Result<()> {
    let Some(domain) = cfg.api.tls.as_ref().map(|tls| tls.acme.domain.clone()) else { return Ok(()) };
    let registered = match candidate {
        Some(candidate) => vault.candidate_hostname_at(&vault.observe(View::Finalized, &[]).await?, candidate).await?,
        None => vault.operator_hostname().await?,
    };
    ensure!(
        registered == domain,
        "the registry names {registered} as the operator in effect's API hostname, but api.tls.acme.domain is \
         {domain}: point the config at the registered hostname, or register the one the config serves"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use engine_core::Fixed;
    use engine_core::types::AccountId;

    use super::*;
    use crate::checkpoints::{LifecycleEvidence, Observation, VaultState};

    /// A chain whose finalized block and registered hostname the test sets; `None` fails the read.
    /// With `turn`, its schedule draws a turn from `BOUNDARY` for a candidate registered at
    /// `TURN_HOST`, while the registry still names the operator registered at `hostname`.
    #[derive(Clone)]
    struct Registry {
        hostname: Arc<Mutex<Option<String>>>,
        height: u64,
        turn: bool,
    }

    const BOUNDARY: Millis = 1_000_000;
    const MEMBER: AccountId = AccountId([0x0c; 20]);
    const TURN_HOST: &str = "member.example";

    impl Registry {
        fn registered(hostname: &str, height: u64) -> Self {
            Self { hostname: Arc::new(Mutex::new(Some(hostname.to_owned()))), height, turn: false }
        }
    }

    impl CheckpointChain for Registry {
        async fn state(&self, _view: View) -> anyhow::Result<VaultState> {
            anyhow::bail!("this chain reads no vault state")
        }

        async fn observe(
            &self,
            view: View,
            _approvals: &[crate::checkpoints::ApprovalId],
        ) -> anyhow::Result<Observation> {
            let state = VaultState {
                height: self.height,
                signer: AccountId([1; 20]),
                checkpoint: None,
                balance: Fixed::ZERO,
                deposits: 0,
                deposited: Fixed::ZERO,
                distributions_paid: Fixed::ZERO,
            };
            let lifecycle = LifecycleEvidence {
                height: self.height,
                block_hash: B256::repeat_byte(0xb1),
                signer: AccountId([1; 20]),
                signer_epoch: 1,
                withdrawals_paused: false,
                total_withdrawn: Fixed::ZERO,
                checkpoint_seq: None,
                checkpoint_root: None,
                approvals: Vec::new(),
            };
            Ok(Observation { view, state, lifecycle: Some(lifecycle) })
        }

        async fn post(&self, _checkpoint: &crate::log::Checkpoint) -> anyhow::Result<()> {
            anyhow::bail!("this chain posts nothing")
        }

        async fn operator_hostname_at(&self, _at: &Observation) -> anyhow::Result<String> {
            self.hostname.lock().unwrap().clone().context("the registry cannot be read")
        }

        async fn candidate_hostname_at(&self, _at: &Observation, candidate: AccountId) -> anyhow::Result<String> {
            anyhow::ensure!(candidate == MEMBER, "{candidate} registered no hostname");
            Ok(TURN_HOST.into())
        }

        async fn scheduled_turn(
            &self,
            _at: &Observation,
            now_ms: Millis,
        ) -> anyhow::Result<Option<crate::checkpoints::ScheduledTurn>> {
            let turn = crate::checkpoints::ScheduledTurn {
                turn: 9,
                boundary: BOUNDARY,
                candidate: MEMBER,
                admin_key: MEMBER,
                release: [0; 32],
                generation: 1,
                set: 1,
                source: None,
            };
            Ok(Some(turn).filter(|turn| self.turn && turn.boundary <= now_ms))
        }
    }

    /// For the finality lag after a boundary the finalized registry still names the holder before
    /// it: the read names the turn in effect at trusted time's member instead, from its boundary on.
    #[tokio::test]
    async fn the_read_names_the_member_of_the_turn_in_effect_at_trusted_time() {
        let chain = Registry { turn: true, ..Registry::registered("holder.example", 42) };
        let before = read(&chain, Some(BOUNDARY - 1)).await.unwrap();
        let after = read(&chain, Some(BOUNDARY)).await.unwrap();
        assert_eq!((before.hostname.as_str(), after.hostname.as_str()), ("holder.example", TURN_HOST));
        assert_eq!(read(&chain, None).await.unwrap().hostname, "holder.example", "no trusted time names a turn");
        let mut cfg = crate::test_support::dev_config();
        let acme =
            "[acme]\ndomain = 'member.example'\ndirectory_url = 'https://ca.test/dir'\ncontact = 'mailto:ops@x.test'";
        cfg.api.tls = Some(toml::from_str(acme).unwrap());
        check_operator_hostname(&cfg, &chain, Some(MEMBER)).await.unwrap();
        assert!(check_operator_hostname(&cfg, &chain, None).await.is_err(), "the holder's hostname passed");
    }

    #[tokio::test]
    async fn the_read_names_the_registrys_hostname_and_the_finalized_block_it_came_from() {
        let registered = read(&Registry::registered("registry.example", 42), None).await.unwrap();
        assert_eq!(
            registered,
            RegisteredHostname { hostname: "registry.example".into(), block: 42, block_hash: B256::repeat_byte(0xb1) }
        );
        let empty = read(&Registry::registered("", 42), None).await.unwrap_err();
        assert!(format!("{empty:#}").contains("records no API hostname"), "{empty:#}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_read_withdraws_the_hostname_with_a_warning_and_a_later_read_restores_it() {
        let chain = Registry::registered("registry.example", 7);
        let (_guard, mut router) = crate::alerts::capture_until_dropped();
        let (feed, served) = tokio::sync::watch::channel(None);
        let task = tokio::spawn(watch_hostname(chain.clone(), Duration::from_secs(60), feed, || None));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(served.borrow().as_ref().map(|read| read.hostname.as_str()), Some("registry.example"));

        *chain.hostname.lock().unwrap() = None;
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(*served.borrow(), None, "an unreadable registry leaves the hostname out");
        let lines = router.take();
        assert_eq!(
            lines.iter().filter(|line| line.name == "operator_hostname_read_failed" && line.severity.is_some()).count(),
            1,
            "{lines:?}"
        );

        *chain.hostname.lock().unwrap() = Some("next.example".into());
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(served.borrow().as_ref().map(|read| read.hostname.as_str()), Some("next.example"));
        let lines = router.take();
        assert!(
            lines.iter().any(|line| line.name == "operator_hostname_read_failed" && line.severity.is_none()),
            "{lines:?}"
        );
        task.abort();
    }
}
