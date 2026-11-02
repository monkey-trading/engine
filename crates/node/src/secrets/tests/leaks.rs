//! Where a value must never go.

use super::*;

// Every path a value can travel, at the most verbose level: what is logged, what a refusal says, and
// what `Debug` prints.
#[tokio::test]
async fn no_value_reaches_a_log_a_refusal_or_debug_output_at_any_level() {
    let written = Written::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(written.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let enclave = Enclave::start_running("secrets-no-leak", Some(None)).await;
    let domain = domain_of(&enclave.cfg);
    let mut reasons = vec![
        enclave.refused(bundle(1, &[("typo_key", "FAKE-unknown")])).await,
        enclave.refused(bundle(1, &[("vendor_key", "FAKE-with space")])).await,
        enclave.refused(bundle(1, &[("critical_url", "FAKE-not-a-url")])).await,
        enclave.refused(bundle(1, &[("critical_url", "ftp://FAKE-host/FAKE-path")])).await,
    ];
    let mut client = enclave.connect().await;
    let delivery = client.encrypt(&full(1, "x").sign(domain, &SigningKey::from_slice(&[0x77; 32]).unwrap()));
    reasons.push(reason(client.send(&delivery).await));
    enclave.sealed(full(1, "x")).await;
    reasons.push(enclave.refused(full(1, "y")).await);
    reasons.push(enclave.refused(full(0, "z")).await);
    enclave.sealed(full(2, "z")).await;

    let signed = full(3, "x").sign(domain, &operator_key());
    let printed = [
        format!("{:?}", signed),
        format!("{:?}", signed.bundle),
        format!("{:?}", signed.bundle.secrets),
        format!("{:?}", enclave.connect().await.offer),
        format!("{:?}", enclave.connect().await.encrypt(&signed)),
    ];
    for text in reasons.iter().chain(&printed) {
        assert!(!text.contains("FAKE"), "{text}");
    }
    let logged = written.text();
    assert!(logged.contains("sealed a delivered secrets bundle"), "the capture missed the node's lines: {logged}");
    assert!(!logged.contains("FAKE"), "{logged}");
}
