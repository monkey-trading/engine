//! The operator route, and the admin route once an election hands the key over, under `V37`.

use super::*;
use engine_core::election::RegistryElection;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::staking::StakingConfig;

/// The access-source test authority's key, which the repository publishes.
const PUBLISHED_KEY: [u8; 32] = [0x77; 32];
const CANDIDATE: &str = "0x00000000000000000000000000000000000000c1";

impl Node {
    /// `start_on` the latest protocol with staking in the dev genesis, which registry mirrors need.
    async fn staking(name: &str) -> Self {
        let mut genesis = dev_config().genesis;
        genesis.staking = Some(StakingConfig {
            liquidity_floor: Fixed::from_int(1_000_000),
            epoch_ms: 86_400_000,
            start_ms: NOW,
            cooldown_ms: 3 * 86_400_000,
        });
        Node::start_with(name, true, &genesis).await
    }

    /// Sequences the registry's rotation switch on, then its election of `admin_key` at the
    /// earliest effective time, returning that time.
    async fn elect(&self, admin_key: AccountId) -> Millis {
        let on =
            Command::SwitchRotation { enabled: true, election_cancelled: false, block: 1, chain_ref: "0xon:0".into() };
        assert_eq!(
            self.state.submit(on).await.unwrap(),
            [Event::RotationSwitched { enabled: true, election_cancelled: false }]
        );
        let effective_at = NOW + NOTICE_MS;
        let election = RegistryElection {
            epoch: 1,
            candidate: CANDIDATE.parse().unwrap(),
            admin_key,
            effective_at,
            block: 2,
            chain_ref: "0xelect:0".into(),
        };
        let events = self.state.submit(Command::ElectOperator(election)).await.unwrap();
        assert!(matches!(events.last(), Some(Event::OperatorElected { .. })), "{events:?}");
        effective_at
    }

    /// Moves the node's clocks past the election's notice.
    async fn pass_the_notice(&mut self) {
        self.stamps.advance(Duration::from_millis(after_the_notice() as u64 - NOW as u64));
        let sample = sample_around(self.stamps.as_ref());
        self.state.tx.send(crate::sequencer::Request::TimeSample(sample)).await.unwrap();
        self.state.clock = after_the_notice;
    }
}

#[tokio::test]
async fn the_operator_route_reports_the_key_its_pending_rotation_and_the_last_election() {
    let node = Node::staking("operator-route").await;
    let (status, body) = node.get("/v1/operator").await;
    let operator = format!("{}", node.state.operator.address);
    assert_eq!(
        (status, body),
        (
            StatusCode::OK,
            serde_json::json!({
                "operator": operator, "pending_rotation": null, "rotation_enabled": false, "last_election": null,
            })
        )
    );

    let elected = engine_core::eip712::address(SigningKey::from_slice(&[0x55; 32]).unwrap().verifying_key());
    let effective_at = node.elect(elected).await;
    let (status, body) = node.get("/v1/operator").await;
    let elected = format!("{elected}");
    assert_eq!(
        (status, body),
        (
            StatusCode::OK,
            serde_json::json!({
                "operator": operator,
                "pending_rotation": { "address": elected, "effective_at": effective_at },
                "rotation_enabled": true,
                "last_election": {
                    "epoch": 1, "candidate": CANDIDATE, "admin_key": elected, "effective_at": effective_at,
                },
            })
        )
    );
}

#[tokio::test]
async fn an_elected_published_key_signs_nothing_on_a_real_enclave() {
    let published = SigningKey::from_slice(&PUBLISHED_KEY).unwrap();
    let address = engine_core::eip712::address(published.verifying_key());
    assert!(crate::config::is_published_operator(address).unwrap());
    for real in [true, false] {
        let mut node = Node::staking(if real { "elected-published" } else { "elected-published-mock" }).await;
        node.elect(address).await;
        node.pass_the_notice().await;
        if real {
            let mut attestation = (*node.state.attestation).clone();
            attestation.provider = "tdx".into();
            node.state.attestation = Arc::new(attestation);
        }
        let request = sign(&published, &node.state.operator, reduce_only(Some(1)), 0, after_the_notice() + 60_000);
        let logged = node.entries().len();
        let (status, body) = node.post("/v1/admin", serde_json::to_value(&request).unwrap()).await;
        if real {
            assert_eq!(
                (status, body["error"].as_str()),
                (StatusCode::UNAUTHORIZED, Some("signed by a published test key"))
            );
            assert_eq!(node.entries().len(), logged);
        } else {
            // A mock enclave takes the published key the election named.
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                body.as_array().and_then(|events| events.last()),
                Some(&serde_json::json!({ "type": "admin_applied" }))
            );
        }
    }
}
