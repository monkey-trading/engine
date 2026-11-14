//! Secrets the config names but never holds. The file's SHA-256 is attested and the file is
//! published, so a credential reaches the node only in a delivered bundle (`crate::secrets`), and the
//! config says which secret each field reads: `{name}` inside a URL's path or query for a provider's
//! key, `api_key_secret` for a vendor's, `oauth.client_id_secret` and `oauth.client_secret_secret`
//! for a vendor's OAuth login, and `url_secret` for an alert webhook's URL.
//!
//! A URL names only its key, never its origin. The custody quorum's providers credit deposits, so
//! the host and scheme stay in the file the timelock approves (the release's config hash), and a
//! delivered value, percent-encoded into its place, can change only what the provider is told, never
//! which provider is asked. An operator who holds the delivery key therefore cannot point the node
//! at a chain of its own.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;

use alloy::transports::http::reqwest::Url;
use anyhow::{Context, bail, ensure};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};

use crate::config::{NodeConfig, TeeProvider};

/// The longest secret name.
const MAX_NAME_BYTES: usize = 64;
/// The longest secret value.
pub(crate) const MAX_VALUE_BYTES: usize = 8 << 10;
/// What a key may keep unencoded in a URL: RFC 3986's unreserved characters, none of which can end
/// the path or query it sits in.
const KEY_IN_URL: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');
/// Stands in for a key while the config's URL is checked: what the file fixes is the same with any
/// key, since encoding keeps a key inside the path or query.
const CHECKED_KEY: &str = "key";
/// A second stand-in, to show what a URL does with a different value.
const OTHER_CHECKED_KEY: &str = "other";

/// The secrets one bundle holds, by name.
pub(crate) type Values = BTreeMap<SecretName, SecretValue>;

/// `[tee.sealing.secrets]`: where this enclave takes delivered secrets.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsSection {
    /// The delivery listener. It speaks the handover's framing with no TLS: the host carries the
    /// connection, so the bundle's encryption and the operator's signature protect it, not the
    /// transport (see `crate::secrets`).
    pub listen: SocketAddr,
}

/// A secret's name: lowercase letters, digits, and `_`, up to 64 bytes. Names are public.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SecretName(String);

/// A secret's value. `Debug` and `Display` hide it; read it with `expose` where it is used.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretValue(String);

/// A URL that may carry credentials, written out or with a `{name}` for a delivered key in its path
/// or query. `Debug` hides it; read it with `expose` where it is used.
#[derive(Deserialize)]
#[serde(transparent)]
pub struct SecretUrl {
    /// The URL as the config writes it.
    text: String,
    /// The delivered key its `{name}` reads, once the node has it.
    #[serde(skip)]
    key: Option<SecretValue>,
}

/// What a field does without its secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SecretUse {
    /// A key in a URL the node cannot safely run without: chain access reads what opens the sealed
    /// keys, a smaller custody quorum would weaken what credits deposits, and no screening source
    /// would admit what it should refuse. The node holds until it has every one.
    Required,
    /// A vendor's API key or OAuth client credentials: the node runs without that connection, and
    /// its markets go unpriced.
    VendorKey,
    /// An alert webhook's URL: the node runs, and those alerts are only logged.
    WebhookUrl,
    /// A key in a seed relayer URL: the node runs, and the relayer stays off.
    SeedRelay,
}

/// A field that names a secret.
#[derive(Clone, Debug)]
pub(crate) struct NamedSecret {
    pub name: SecretName,
    /// The config field, for alerts.
    pub field: String,
    pub usage: SecretUse,
}

/// A URL split around its `{name}`.
struct Template<'a> {
    before: &'a str,
    name: SecretName,
    after: &'a str,
}

impl TryFrom<String> for SecretName {
    type Error = String;

    /// Checks the grammar, never quoting the input.
    fn try_from(name: String) -> Result<Self, String> {
        let legal = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_';
        if name.is_empty() || name.len() > MAX_NAME_BYTES || !name.bytes().all(legal) {
            return Err(format!(
                "a secret name is 1 to {MAX_NAME_BYTES} bytes of lowercase letters, digits, and underscores"
            ));
        }
        Ok(Self(name))
    }
}

impl From<SecretName> for String {
    /// The name's text.
    fn from(name: SecretName) -> Self {
        name.0
    }
}

impl fmt::Display for SecretName {
    /// The name, which is public.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for SecretValue {
    /// Never prints the value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue(<redacted>)")
    }
}

impl fmt::Display for SecretValue {
    /// Never prints the value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Debug for SecretUrl {
    /// Never prints the URL.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretUrl(<redacted>)")
    }
}

impl SecretValue {
    /// The value, for the one place that uses it.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// Checks a value a bundle carries. A stray newline from a copied key would otherwise only show
    /// as a vendor's refusal, and a value of dots would climb out of the path it is put in.
    /// Error cases:
    /// - it is empty, over `MAX_VALUE_BYTES`, not printable ASCII without spaces, or only dots
    pub(crate) fn check(&self, name: &SecretName) -> anyhow::Result<()> {
        ensure!(
            !self.0.is_empty() && self.0.len() <= MAX_VALUE_BYTES,
            "secret {name} is empty or over {MAX_VALUE_BYTES} bytes"
        );
        ensure!(self.0.bytes().all(|b| b.is_ascii_graphic()), "secret {name} is not printable ASCII without spaces");
        ensure!(!self.0.bytes().all(|b| b == b'.'), "secret {name} is only dots");
        Ok(())
    }

    /// A value from a test's text.
    #[cfg(test)]
    pub(crate) fn test(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl SecretUrl {
    /// The URL, for the one place that connects with it.
    /// Error cases:
    /// - the value is not a valid URL, or its `{name}` has not been delivered
    pub fn expose(&self) -> anyhow::Result<Url> {
        let Some(template) = self.template()? else { return parse(&self.text) };
        let key = self.key.as_ref().with_context(|| format!("secret {} has not been delivered", template.name))?;
        let url = template.with(key.expose())?;
        ensure!(
            authority_of(&url) == authority_of(&template.with(CHECKED_KEY)?),
            "secret {} would change which server this URL asks",
            template.name
        );
        Ok(url)
    }

    /// Every form of what this URL must keep out of a diagnostic: the URL, its path, query, and
    /// password on their own, since transport errors quote them and providers put keys in them, and a
    /// delivered key raw and encoded, since a provider's refusal may echo it alone. Longest first, so
    /// a short form cannot split a longer one before it is matched, and none empty, which `redact`
    /// would otherwise insert between every character.
    /// Error cases:
    /// - see `expose`
    pub(crate) fn redactions(&self) -> anyhow::Result<Vec<String>> {
        let url = self.expose()?;
        let parts = [Some(url.as_str()), Some(url.path()).filter(|path| path.len() > 1), url.query(), url.password()];
        let mut forms: Vec<String> = parts.into_iter().flatten().map(str::to_owned).collect();
        if let Some(key) = &self.key {
            forms.push(key.expose().to_owned());
            forms.push(utf8_percent_encode(key.expose(), KEY_IN_URL).to_string());
        }
        forms.retain(|form| !form.is_empty());
        forms.sort_by_key(|form| std::cmp::Reverse(form.len()));
        forms.dedup();
        Ok(forms)
    }

    /// The URL the attested file fixes, for the checks a config load makes: the same scheme, host,
    /// and port `expose` connects to, with a stand-in for any delivered key.
    /// Error cases:
    /// - the value is not a valid URL, or its `{name}` is malformed
    pub(crate) fn attested(&self) -> anyhow::Result<Url> {
        match self.template()? {
            Some(template) => template.with(CHECKED_KEY),
            None => parse(&self.text),
        }
    }

    /// The secret this URL reads, if it reads one.
    /// Error cases:
    /// - its `{name}` is malformed
    fn secret_name(&self) -> anyhow::Result<Option<SecretName>> {
        Ok(self.template()?.map(|template| template.name))
    }

    /// Whether this URL reads a delivered key.
    /// Error cases:
    /// - its `{name}` is malformed
    pub(crate) fn names_a_secret(&self) -> anyhow::Result<bool> {
        Ok(self.secret_name()?.is_some())
    }

    /// Sets a delivered key, as a test's provider would be given it.
    #[cfg(test)]
    pub(crate) fn deliver_test_key(&mut self, value: &str) {
        self.key = Some(SecretValue::test(value));
    }

    /// Sets the key `values` holds for this URL's `{name}`.
    /// Error cases:
    /// - its `{name}` is malformed
    fn deliver(&mut self, values: &Values) -> anyhow::Result<()> {
        if let Some(name) = self.secret_name()? {
            self.key = values.get(&name).cloned();
        }
        Ok(())
    }

    /// The URL split around its `{name}`, if it has one. Errors never quote the text, which may hold
    /// a credential.
    /// Error cases:
    /// - a brace without its partner, a second placeholder, a malformed name, or a placeholder
    ///   anywhere but the path or query, where a value could change which server is asked
    fn template(&self) -> anyhow::Result<Option<Template<'_>>> {
        let Some((before, rest)) = self.text.split_once('{') else {
            ensure!(!self.text.contains('}'), "a URL has a closing brace without an opening one");
            return Ok(None);
        };
        let (name, after) = rest.split_once('}').context("a URL's secret placeholder is not closed")?;
        ensure!(!after.contains(['{', '}']), "a URL may name one secret");
        let name = SecretName::try_from(name.to_owned()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let outside = |name: &SecretName| {
            anyhow::anyhow!(
                "the secret {name} must sit in a URL's path or query: the attested config fixes which server is asked"
            )
        };
        let authority = before.split_once("://").map_or("", |(_, authority)| authority);
        if !authority.contains(['/', '?']) || before.contains('#') {
            return Err(outside(&name));
        }
        // The text alone cannot say where a value lands, since a URL parser skips extra slashes and
        // takes a backslash for one: `https:///{key}` makes it the host. So parse with two values,
        // and refuse a URL whose server or credentials follow them.
        let template = Template { before, name, after };
        let (one, other) = (template.with(CHECKED_KEY)?, template.with(OTHER_CHECKED_KEY)?);
        if authority_of(&one) != authority_of(&other) || one.fragment() != other.fragment() {
            return Err(outside(&template.name));
        }
        Ok(Some(template))
    }
}

impl Template<'_> {
    /// The URL with `key` in place of its `{name}`.
    /// Error cases:
    /// - the result is not a valid URL
    fn with(&self, key: &str) -> anyhow::Result<Url> {
        parse(&format!("{}{}{}", self.before, utf8_percent_encode(key, KEY_IN_URL), self.after))
    }
}

impl NodeConfig {
    /// Every field that names a secret.
    /// Error cases:
    /// - a URL's `{name}` is malformed
    pub(crate) fn named_secrets(&self) -> anyhow::Result<Vec<NamedSecret>> {
        let mut named = Vec::new();
        let mut url = |url: &SecretUrl, field: String| -> anyhow::Result<()> {
            if let Some(name) = url.secret_name().with_context(|| field.clone())? {
                named.push(NamedSecret { name, field, usage: SecretUse::Required });
            }
            Ok(())
        };
        if let Some(checkpoints) = &self.checkpoints {
            url(&checkpoints.rpc_url, "checkpoints.rpc_url".to_owned())?;
        }
        if let Some(evm) = &self.custody.evm {
            for provider in &evm.providers {
                url(&provider.rpc_url, format!("custody.evm.providers[{}].rpc_url", provider.name))?;
            }
        }
        if let Some(access) = &self.access {
            url(&access.endpoint, "access.endpoint".to_owned())?;
        }
        for connection in self.feeds.venues.iter().flat_map(|venues| &venues.connections) {
            let oauth = connection.oauth.iter().flat_map(|oauth| {
                [
                    ("oauth.client_id_secret", &oauth.client_id_secret),
                    ("oauth.client_secret_secret", &oauth.client_secret_secret),
                ]
            });
            for (key, name) in connection.api_key_secret.iter().map(|name| ("api_key_secret", name)).chain(oauth) {
                let field = format!("feeds.venues.connections[{}].{key}", connection.url);
                named.push(NamedSecret { name: name.clone(), field, usage: SecretUse::VendorKey });
            }
        }
        for (key, route) in [("critical", &self.alerts.critical), ("warning", &self.alerts.warning)] {
            if let Some(name) = route.as_ref().and_then(|route| route.url_secret.as_ref()) {
                let field = format!("alerts.{key}.url_secret");
                named.push(NamedSecret { name: name.clone(), field, usage: SecretUse::WebhookUrl });
            }
        }
        if let Some(relay) = &self.seed_relay {
            for (url, field) in relay.urls() {
                if let Some(name) = url.secret_name().with_context(|| field.clone())? {
                    named.push(NamedSecret { name, field, usage: SecretUse::SeedRelay });
                }
            }
        }
        Ok(named)
    }

    /// Sets every URL's delivered key from `values`; a key it does not hold stays undelivered, which
    /// `expose` refuses.
    /// Error cases:
    /// - a URL's `{name}` is malformed, which the config load refused
    pub(crate) fn deliver_urls(&mut self, values: &Values) -> anyhow::Result<()> {
        if let Some(checkpoints) = &mut self.checkpoints {
            checkpoints.rpc_url.deliver(values)?;
        }
        if let Some(evm) = &mut self.custody.evm {
            for provider in &mut evm.providers {
                provider.rpc_url.deliver(values)?;
            }
        }
        if let Some(access) = &mut self.access {
            access.endpoint.deliver(values)?;
        }
        if let Some(relay) = &mut self.seed_relay {
            for provider in &mut relay.providers {
                provider.rpc_url.deliver(values)?;
            }
            relay.rpc_url.deliver(values)?;
            relay.beacon_api.deliver(values)?;
        }
        Ok(())
    }

    /// Checks the fields that name secrets, for the node a config with `[standby]` may be started as.
    /// Error cases:
    /// - see `check_secrets`, with a standby's vendor keys exempt when the config has `[standby]`
    pub(super) fn validate_secrets(&self) -> anyhow::Result<()> {
        self.check_secrets(self.standby.is_some())
    }

    /// `validate_secrets` for a node about to run, whatever its config's `[standby]` says: only
    /// `--standby` runs as one, and a node that runs feeds must be able to receive their keys.
    /// Error cases:
    /// - see `check_secrets`
    pub(crate) fn validate_secrets_on_a_node(&self) -> anyhow::Result<()> {
        self.check_secrets(false)
    }

    /// Checks the fields that name secrets. A standby runs no feeds, so it never reads a vendor's
    /// key, and `as_standby` leaves those fields out of the check that something can deliver them;
    /// the standby's other secrets it reads, so they keep their listener or their written value.
    /// Error cases:
    /// - a URL's `{name}` is malformed
    /// - an alert route names neither or both of a variable and a secret
    /// - a field names a secret and `[tee.sealing.secrets]` is missing, so nothing could deliver it
    /// - `[tee.sealing.secrets]` is set with real attestation and no `tee.processor`, so no offer's
    ///   quote could carry the collateral that verifies it, and a node holding for its secrets would
    ///   never reach a later check
    fn check_secrets(&self, as_standby: bool) -> anyhow::Result<()> {
        for (key, route) in [("critical", &self.alerts.critical), ("warning", &self.alerts.warning)] {
            if let Some(route) = route {
                ensure!(
                    route.url_env.is_some() != route.url_secret.is_some(),
                    "alerts.{key} names exactly one of url_env and url_secret"
                );
            }
        }
        let named = self.named_secrets()?;
        let listens = self.tee.sealing.as_ref().is_some_and(|sealing| sealing.secrets.is_some());
        ensure!(
            !listens || self.tee.provider == TeeProvider::Mock || self.tee.processor.is_some(),
            "[tee.sealing.secrets] with real attestation needs tee.processor, to fetch its quotes' collateral"
        );
        if let Some(first) = named.iter().find(|field| !(as_standby && field.usage == SecretUse::VendorKey))
            && !listens
        {
            bail!("{} names a delivered secret, which needs [tee.sealing.secrets] to receive it", first.field);
        }
        Ok(())
    }

    /// Refuses at load what `refuse_environment_secrets` does, but for a config with `[standby]`,
    /// which `--standby` runs on its operator's own host, in no enclave. A node started on that config
    /// without the flag is refused by `refuse_environment_secrets_on_a_node`.
    /// Error cases:
    /// - see `refuse_environment_secrets`
    pub(super) fn refuse_environment_secrets_at_load(&self, production: bool) -> anyhow::Result<()> {
        if self.standby.is_some() {
            return Ok(());
        }
        self.refuse_environment_secrets(production)
    }

    /// `refuse_environment_secrets` for this build, for a node about to run, whatever its config's
    /// `[standby]` says: only `--standby` runs as one.
    /// Error cases:
    /// - see `refuse_environment_secrets`
    pub(crate) fn refuse_environment_secrets_on_a_node(&self) -> anyhow::Result<()> {
        self.refuse_environment_secrets(super::PRODUCTION_BUILD)
    }

    /// Refuses a secret read from the environment when `production` is set: the guest's only
    /// environment is the measured kernel command line, so such a field could never be set, and a
    /// host that could set one would choose the secret.
    /// Error cases:
    /// - `production` is set and `alerts.critical.url_env`, `alerts.warning.url_env`, or a vendor
    ///   connection's `api_key_env` is set
    fn refuse_environment_secrets(&self, production: bool) -> anyhow::Result<()> {
        let routes =
            [("alerts.critical.url_env", &self.alerts.critical), ("alerts.warning.url_env", &self.alerts.warning)];
        let mut fields: Vec<String> = routes
            .into_iter()
            .filter(|(_, route)| route.as_ref().is_some_and(|route| route.url_env.is_some()))
            .map(|(field, _)| field.to_owned())
            .collect();
        fields.extend(
            self.feeds
                .venues
                .iter()
                .flat_map(|venues| &venues.connections)
                .filter(|connection| connection.api_key_env.is_some())
                .map(|connection| format!("{} api_key_env", connection.venue.name())),
        );
        if production && !fields.is_empty() {
            bail!(
                "a production build reads secrets only from a delivered bundle, not the environment: {}",
                fields.join(", ")
            );
        }
        Ok(())
    }
}

/// `text` with every one of `forms` redacted.
pub(crate) fn redact(text: &str, forms: &[String]) -> String {
    forms.iter().fold(text.to_owned(), |text, form| text.replace(form.as_str(), "<redacted>"))
}

/// Who a URL asks and as whom: everything a delivered value must not reach.
fn authority_of(url: &Url) -> (&str, Option<&str>, Option<u16>, &str, Option<&str>) {
    (url.scheme(), url.host_str(), url.port(), url.username(), url.password())
}

/// `text` parsed. `url::ParseError` names the problem, never the input, so nothing secret reaches a
/// log.
/// Error cases:
/// - `text` is not a URL
fn parse(text: &str) -> anyhow::Result<Url> {
    Url::parse(text).map_err(|e| anyhow::anyhow!("not a URL: {e}"))
}

#[cfg(test)]
mod tests;
