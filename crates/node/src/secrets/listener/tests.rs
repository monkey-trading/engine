//! What a test cannot reach through a mock enclave's connection: a real enclave's refusal, and the
//! bound on connections.

use k256::ecdsa::SigningKey;

use super::*;
use crate::secrets::tests::{CONFIG, Enclave, RELEASE, config, domain_of, full, operator_key};
use crate::tee::MockAttestor;
use crate::tee::sealing::{MockSealer, Sealer};

/// `signing`, as the key in effect, sealing a delivery on an enclave of `provider`, its connection
/// past the quote a real one would need hardware for.
async fn sealing(name: &str, provider: TeeProvider, signing: &SigningKey) -> anyhow::Result<Receipt> {
    let mut cfg = config(name);
    cfg.tee.provider = provider;
    let sealer: Box<dyn Sealer> = Box::new(MockSealer { launch_digest: RELEASE });
    let platform = sealer.platform().unwrap();
    let dir = &cfg.tee.sealing.as_ref().unwrap().dir;
    let store = Arc::new(SecretStore::open(dir, sealer, CONFIG).unwrap());
    let release = store.release();
    let signer = engine_core::eip712::address(signing.verifying_key());
    let (_authority, taking) = watch::channel(Authority::Configured(signer));
    let listen = "127.0.0.1:0".parse().unwrap();
    let named = cfg.named_secrets().unwrap();
    let listener =
        Listener::bind(&cfg, listen, platform, Box::new(MockAttestor), store, named, taking.clone()).await.unwrap();
    let (secret, public) = Kx::gen_keypair();
    let (domain, nonce) = (domain_of(&cfg), [0x5c; 32]);
    let signed = full(1, "a").sign(domain, signing);
    let delivery = delivery::encrypt(domain, &release, &public.to_bytes(), &nonce, &signed).unwrap();
    listener.deliveries.seal_delivery(&secret, &nonce, &delivery, &taking).await
}

// Anyone can hold a published key, and an election can name one, so it seals nothing on real hardware.
#[tokio::test]
async fn a_real_enclave_refuses_a_published_signer_and_the_mock_takes_it() {
    let published = operator_key();
    assert!(is_published_operator(engine_core::eip712::address(published.verifying_key())).unwrap());
    let err = sealing("listener-published", TeeProvider::ConfigfsTsm, &published).await.unwrap_err();
    assert!(err.to_string().contains("a published test key"), "{err:#}");
    sealing("listener-published-mock", TeeProvider::Mock, &published).await.unwrap();
    let own = SigningKey::from_slice(&[0x55; 32]).unwrap();
    sealing("listener-own", TeeProvider::ConfigfsTsm, &own).await.unwrap();
}

// The cap bounds the quotes strangers can cause: past it a hello goes unanswered until one ends.
#[tokio::test]
async fn a_connection_past_the_cap_waits_for_one_to_end() {
    let enclave = Enclave::start("listener-cap").await;
    let mut held = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        held.push(TcpStream::connect(&enclave.address).await.unwrap());
    }
    // Let the accept loop take them all before the one that must wait.
    sleep(Duration::from_millis(100)).await;
    let mut waiting = TcpStream::connect(&enclave.address).await.unwrap();
    send(&mut waiting, &Hello { nonce: hex::encode([0x5c; 32]) }).await.unwrap();
    assert!(
        timeout(Duration::from_millis(300), receive::<Offer>(&mut waiting)).await.is_err(),
        "a connection past the cap was served"
    );

    held.pop();
    let offer = timeout(Duration::from_secs(5), receive::<Offer>(&mut waiting)).await;
    offer.expect("no connection took the freed slot").unwrap();
}
