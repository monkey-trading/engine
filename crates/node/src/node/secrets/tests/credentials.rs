//! What a venue connection authenticates with: nothing for a public venue, and for a connection that
//! names two secrets, its credentials only once both are delivered.

use super::*;

use crate::config::SecretValue;
use crate::feeds::venue::tls_config;

#[tokio::test]
async fn a_public_venue_is_never_withheld() {
    let cfg = config("startup-env");
    let delivered = Delivered::default();
    let mut connection = cfg.feeds.venues.as_ref().unwrap().connections[0].clone();
    connection.api_key_secret = None;
    assert!(!delivered.withholds(&connection), "no secret to lack");
    assert!(delivered.api_key(&connection).unwrap().is_none(), "a public venue has no key");
}

#[tokio::test]
async fn an_oauth_connection_is_withheld_until_both_its_secrets_are_delivered() {
    let mut connection = config("startup-oauth").feeds.venues.as_ref().unwrap().connections[0].clone();
    connection.api_key_secret = None;
    let oauth = "token_url = 'https://auth.cmegroup.com/as/token.oauth2'\nclient_id_secret = 'cme_id'\n\
                 client_secret_secret = 'cme_secret'";
    connection.oauth = Some(toml::from_str(oauth).unwrap());
    let tls = tls_config().unwrap();
    let name = |name: &str| SecretName::try_from(name.to_owned()).unwrap();
    // Either secret alone leaves the connection off.
    for (only, value) in [("cme_id", "FAKE-id"), ("cme_secret", "FAKE-secret")] {
        let mut partial = Delivered::default();
        partial.values.insert(name(only), SecretValue::test(value));
        assert!(partial.withholds(&connection), "only {only} is delivered");
        assert!(partial.credentials(&connection, &tls, Duration::from_secs(1)).is_err());
    }
    let mut delivered = Delivered::default();
    delivered.values.insert(name("cme_id"), SecretValue::test("FAKE-id"));
    delivered.values.insert(name("cme_secret"), SecretValue::test("FAKE-secret"));
    assert!(!delivered.withholds(&connection));
    let credentials = delivered.credentials(&connection, &tls, Duration::from_secs(1)).unwrap();
    assert!(matches!(credentials, Credentials::OAuth(_)), "{credentials:?}");
    assert!(!format!("{credentials:?}").contains("FAKE"), "{credentials:?}");
}

#[tokio::test]
async fn a_key_pair_connection_is_withheld_until_both_its_secrets_are_delivered() {
    let mut connection = config("startup-key-pair").feeds.venues.as_ref().unwrap().connections[0].clone();
    connection.api_key_secret = None;
    let pair = "key_id_secret = 'alpaca_key_id'\nsecret_key_secret = 'alpaca_secret_key'";
    connection.key_pair = Some(toml::from_str(pair).unwrap());
    let tls = tls_config().unwrap();
    let name = |name: &str| SecretName::try_from(name.to_owned()).unwrap();
    // Either secret alone leaves the connection off.
    for (only, value) in [("alpaca_key_id", "FAKE-key-id"), ("alpaca_secret_key", "FAKE-secret-key")] {
        let mut partial = Delivered::default();
        partial.values.insert(name(only), SecretValue::test(value));
        assert!(partial.withholds(&connection), "only {only} is delivered");
        assert!(partial.credentials(&connection, &tls, Duration::from_secs(1)).is_err());
    }
    let mut delivered = Delivered::default();
    delivered.values.insert(name("alpaca_key_id"), SecretValue::test("FAKE-key-id"));
    delivered.values.insert(name("alpaca_secret_key"), SecretValue::test("FAKE-secret-key"));
    assert!(!delivered.withholds(&connection));
    let credentials = delivered.credentials(&connection, &tls, Duration::from_secs(1)).unwrap();
    let Credentials::KeyPair { key_id, secret_key } = &credentials else { panic!("{credentials:?}") };
    assert_eq!((key_id.expose(), secret_key.expose()), ("FAKE-key-id", "FAKE-secret-key"));
    assert!(!format!("{credentials:?}").contains("FAKE"), "{credentials:?}");
}
