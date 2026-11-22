//! What a test cannot reach through a mock enclave's connection: a real enclave's refusal, the
//! bounds on connections, and the authority a following member's listener answers under.

use engine_core::auth::Operator;
use engine_core::command::Command;
use engine_core::eip712::B256;
use engine_core::election::RegistryInstallation;
use engine_core::event::Event;
use k256::ecdsa::SigningKey;

use super::*;
use crate::secrets::tests::{CONFIG, Enclave, RELEASE, config, domain_of, full, operator_key};
use crate::sequencer::tests::operator::{recover_turns, turn_log};
use crate::tee::MockAttestor;
use crate::tee::sealing::{MockSealer, Sealer};
use crate::test_support::{dev_config, sample_around};

/// `signing`, as the key in effect, sealing a delivery on an enclave of `provider`, its connection
/// past the quote a real one would need hardware for.
async fn sealing(name: &str, provider: TeeProvider, signing: &SigningKey) -> anyhow::Result<Receipt> {
    let signer = engine_core::eip712::address(signing.verifying_key());
    let (_authority, taking) = watch::channel(Authority::Configured(signer));
    deliver(name, provider, signing, taking).await
}

/// A delivery `signing` signs, sealed on an enclave of `provider` whose listener takes deliveries
/// from whom `taking` says.
async fn deliver(
    name: &str,
    provider: TeeProvider,
    signing: &SigningKey,
    taking: watch::Receiver<Authority>,
) -> anyhow::Result<Receipt> {
    let mut cfg = config(name);
    cfg.tee.provider = provider;
    let sealer: Box<dyn Sealer> = Box::new(MockSealer { launch_digest: RELEASE });
    let platform = sealer.platform().unwrap();
    let dir = &cfg.tee.sealing.as_ref().unwrap().dir;
    let store = Arc::new(SecretStore::open(dir, sealer, CONFIG).unwrap());
    let release = store.release();
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

/// After a confirmed handover the stopped sequencer's authority answers no delivery; once the run
/// hands the listener back to the member's own operator key, a delivery that key signs is taken.
#[tokio::test]
async fn a_delivery_after_a_confirmed_handover_is_taken_from_the_members_own_key() {
    let signing = SigningKey::from_slice(&[0x55; 32]).unwrap();
    let signer = engine_core::eip712::address(signing.verifying_key());
    let (stopped, _) = mpsc::channel(1);
    let clock = Arc::new(crate::clock::SystemClock);
    let trusted = Arc::new(TrustedClock::new(clock, watch::channel(None).1, Duration::from_secs(1)));
    let log = Authority::Log { tx: stopped.downgrade(), configured: signer, installed: None, trusted };
    let (authority, taking) = watch::channel(log);
    drop(stopped);
    let err = deliver("listener-handed-over", TeeProvider::Mock, &signing, taking.clone()).await.unwrap_err();
    assert!(format!("{err:#}").contains("the node is stopping"), "{err:#}");

    let delivered = crate::node::secrets::Delivered { authority: Some(authority), ..Default::default() };
    delivered.following(signer);
    deliver("listener-following", TeeProvider::Mock, &signing, taking).await.expect("the member's own key was refused");
}

// Peers that took their offers and wait hold no place from a new connection: the oldest waiting one
// gives its place up.
#[tokio::test]
async fn a_full_listener_gives_the_oldest_waiting_place_to_a_new_connection() {
    let enclave = Enclave::start("listener-full").await;
    let mut held = Vec::new();
    for _ in 0..CONNECTIONS {
        held.push(enclave.connect().await);
    }
    let mut stream = TcpStream::connect(&enclave.address).await.unwrap();
    send(&mut stream, &Hello { nonce: hex::encode([0x5c; 32]) }).await.unwrap();
    let offer = timeout(HELLO_TIMEOUT, receive::<Offer>(&mut stream)).await;
    offer.expect("a full listener left a new connection no place").unwrap();
    let oldest = timeout(Duration::from_secs(5), receive::<Reply>(&mut held[0].stream)).await;
    assert!(oldest.is_ok_and(|read| read.is_err()), "the oldest waiting connection kept its place");
    let newest = timeout(Duration::from_millis(100), receive::<Reply>(&mut held[CONNECTIONS - 1].stream)).await;
    assert!(newest.is_err(), "a newer waiting connection lost its place");
}

// One address holds only its share of the places: a third connection from it takes the place of its
// oldest silent one, and the other goes at its hello's time.
#[tokio::test]
async fn one_address_holds_at_most_its_share_and_only_until_its_hello_is_due() {
    let (address, task) = listening("listener-per-peer", Box::new(MockAttestor), false).await;
    let mut silent = Vec::new();
    for _ in 0..PER_PEER {
        silent.push(TcpStream::connect(address).await.unwrap());
    }
    // Accepted in the order they connect, so the third is the one past the share.
    hello(address).await.expect("a third connection from the address got no offer").unwrap();
    let oldest = timeout(Duration::from_secs(1), receive::<Offer>(&mut silent[0])).await;
    assert!(oldest.is_ok_and(|read| read.is_err()), "the address's oldest silent connection kept its place");
    let newer = timeout(HELLO_TIMEOUT + Duration::from_secs(5), receive::<Offer>(&mut silent[1])).await;
    assert!(newer.is_ok_and(|read| read.is_err()), "a silent connection outlived its hello's time");
    task.abort();
}

// However many strangers say hello, at most `QUOTES` connections quote at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn at_most_a_set_number_of_connections_quote_at_once() {
    let gated = Arc::new(Gated::default());
    // Whatever fails below, the quotes waiting on the gate are let through.
    let _opens = Opens(gated.clone());
    let (address, task) = listening("listener-quotes", Box::new(gated.clone()), true).await;
    let mut hellos = (0..=QUOTES).map(|_| tokio::spawn(hello(address))).collect::<Vec<_>>();
    let quoting = async {
        while gated.inside.load(Ordering::SeqCst) < QUOTES {
            sleep(Duration::from_millis(10)).await;
        }
    };
    timeout(Duration::from_secs(10), quoting).await.expect("the hellos never filled the quotes");
    sleep(Duration::from_millis(300)).await;
    assert_eq!(gated.peak.load(Ordering::SeqCst), QUOTES, "more connections quoted at once than the limit");
    gated.open();
    for answered in hellos.drain(..) {
        answered.await.unwrap().expect("a hello waited past its bound").expect("no offer came");
    }
    task.abort();
}

// Under daily turns a following member's enclave takes deliveries from its own operator's key alone,
// never from the turn member's key the log records: with rotation on, and as much once rotation is
// turned off while the drawn turn stands. The installation that ends the turns lifts that.
#[tokio::test]
async fn a_member_takes_no_delivery_from_the_turns_key_while_a_drawn_turn_stands_with_rotation_off() {
    let (dir, clock, _, member) = turn_log("listener-turns-off");
    let operator = Operator { address: member, ..dev_config().operator().unwrap() };
    let mut seq = recover_turns(&dir, clock.clone(), operator, B256::repeat_byte(0x51));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let (tx, rx) = mpsc::channel(4);
    let run = tokio::spawn(seq.run(rx));
    let submit = async |cmd| {
        let (reply, events) = oneshot::channel();
        tx.send(Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
        events.await.unwrap()
    };
    // The turn's first command takes it, and its member's key applies.
    submit(Command::Tick).await;
    let own = AccountId([0xd2; 20]);
    let trusted = Arc::new(crate::test_support::trusted(Arc::new(crate::clock::SystemClock)));
    let authority = Authority::Log { tx: tx.downgrade(), configured: own, installed: None, trusted };
    assert!(!authority.admits(member).await.unwrap(), "the turn member's key reached another member's enclave");
    assert!(authority.admits(own).await.unwrap(), "the member's own key was refused");

    let off =
        Command::SwitchRotation { enabled: false, election_cancelled: false, block: 14, chain_ref: "0x04:0".into() };
    let events = submit(off).await;
    assert!(!events.iter().any(|event| matches!(event, Event::ElectionRejected { .. })), "{events:?}");
    assert!(
        !authority.admits(member).await.unwrap(),
        "the turn member's key reached another member's enclave with rotation off"
    );
    assert!(authority.admits(own).await.unwrap(), "the member's own key was refused with rotation off");

    let installation = RegistryInstallation {
        candidate: AccountId([0xc9; 20]),
        admin_key: AccountId([0xd9; 20]),
        block: 16,
        chain_ref: "0x06:0".into(),
    };
    let events = submit(Command::InstallOperator(installation)).await;
    assert!(events.iter().any(|event| matches!(event, Event::OperatorInstalled { .. })), "{events:?}");
    assert!(authority.admits(member).await.unwrap(), "the key the log records was refused once the turns ended");
    drop((authority, tx));
    run.await.unwrap().unwrap();
}

// ── Helpers ──

/// A delivery listener quoting with `attestor` on a free loopback port, running, letting one address
/// hold any number of connections when `loopback`: its address and task.
async fn listening(
    name: &str,
    attestor: Box<dyn Attestor>,
    loopback: bool,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let (_authority, taking) = watch::channel(Authority::Configured(AccountId([1; 20])));
    let cfg = config(name);
    let sealer: Box<dyn Sealer> = Box::new(MockSealer { launch_digest: RELEASE });
    let platform = sealer.platform().unwrap();
    let store = Arc::new(SecretStore::open(&cfg.tee.sealing.as_ref().unwrap().dir, sealer, CONFIG).unwrap());
    let (listen, named) = ("127.0.0.1:0".parse().unwrap(), cfg.named_secrets().unwrap());
    let listener = Listener::bind(&cfg, listen, platform, attestor, store, named, taking).await.unwrap();
    let listener = if loopback { listener.loopback() } else { listener };
    (listener.local_addr(), tokio::spawn(listener.run()))
}

/// The offer a new connection to `address` gets for its hello, within the hello's time.
async fn hello(address: std::net::SocketAddr) -> Result<anyhow::Result<Offer>, tokio::time::error::Elapsed> {
    let mut stream = TcpStream::connect(address).await.unwrap();
    send(&mut stream, &Hello { nonce: hex::encode([0x5c; 32]) }).await.unwrap();
    timeout(HELLO_TIMEOUT + Duration::from_secs(10), receive::<Offer>(&mut stream)).await
}

/// A mock enclave whose quotes wait until it opens, counting those inside at once and the most there were.
#[derive(Default)]
struct Gated {
    inside: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
    opened: (std::sync::Mutex<bool>, std::sync::Condvar),
}

impl Gated {
    /// Lets every quote through.
    fn open(&self) {
        *self.opened.0.lock().unwrap() = true;
        self.opened.1.notify_all();
    }
}

/// Opens the gate when dropped.
struct Opens(Arc<Gated>);

impl Drop for Opens {
    fn drop(&mut self) {
        self.0.open();
    }
}

impl Attestor for Arc<Gated> {
    fn attest(&self, report_data: [u8; 64]) -> anyhow::Result<(String, Vec<u8>)> {
        let inside = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(inside, Ordering::SeqCst);
        // Handed off, so the tasks this worker holds run elsewhere while it waits.
        tokio::task::block_in_place(|| {
            let opened = self.opened.0.lock().unwrap();
            drop(self.opened.1.wait_while(opened, |opened| !*opened).unwrap());
        });
        self.inside.fetch_sub(1, Ordering::SeqCst);
        MockAttestor.attest(report_data)
    }
}
