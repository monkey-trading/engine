//! The `[api.tls]` section: a public CA certificate for the API's domain, issued over ACME for the
//! key the enclave generated and the attestation names.

use std::time::Duration;

use anyhow::{Context, bail, ensure};
use reqwest::Url;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use serde::Deserialize;

use crate::config::{ApiSection, TeeProvider};

/// The least `renew_before`: renewal then starts before a failure raises `certificate_expiring`
/// and before the operator's Prometheus pages at 7 days left, and long before a CA's backdated
/// `notBefore` (Let's Encrypt's, by an hour) could push it past expiry.
const MIN_RENEW_BEFORE: Duration = Duration::from_secs(14 * 86_400);

/// TLS for the API beyond its in-enclave key and self-signed certificate.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiTlsSection {
    pub acme: AcmeSection,
}

/// An ACME CA that issues the API's certificate, validated by TLS-ALPN-01 on the API's own
/// listener. Setting it agrees to the CA's terms of service.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcmeSection {
    /// The API's DNS name, lowercase: the certificate's only identifier. The CA connects to it on
    /// port 443, which the host forwards unmodified to `node.listen`.
    pub domain: String,
    /// The CA's ACME directory, over https.
    pub directory_url: String,
    /// The ACME account's contact, a `mailto:` URL.
    pub contact: String,
    /// Renews once this little of a certificate's life remains, at least `MIN_RENEW_BEFORE`; one
    /// third of its life when unset.
    #[serde(default, with = "humantime_serde")]
    pub renew_before: Option<Duration>,
    /// A local test CA, such as Pebble: its directory's certificate chains to `root`, which then
    /// replaces the compiled-in Mozilla roots. Mock TEE only.
    pub test_ca: Option<AcmeTestCa>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcmeTestCa {
    /// The test CA's root certificate, PEM.
    pub root: String,
}

impl ApiSection {
    /// Refuses, in a production build, an API that would serve only its self-signed certificate,
    /// which no browser trusts.
    /// Error cases:
    /// - `production` is set and `[api.tls.acme]` is missing
    pub(super) fn refuse_self_signed(&self, production: bool) -> anyhow::Result<()> {
        if production && self.tls.is_none() {
            bail!("a production build refuses the self-signed API certificate: [api.tls.acme] is required");
        }
        Ok(())
    }
}

impl AcmeSection {
    /// Checks the values a CA would refuse, or that would make every renewal fail.
    /// Error cases:
    /// - `domain` is not a lowercase DNS name of two labels or more, is a wildcard, or ends in a dot
    /// - `directory_url` is not an https URL
    /// - `contact` is not a `mailto:` URL
    /// - `renew_before` is shorter than `MIN_RENEW_BEFORE`
    /// - `test_ca` is set with a TEE other than the mock, or its root is not one PEM certificate
    pub(super) fn validate(&self, tee: TeeProvider) -> anyhow::Result<()> {
        let domain = &self.domain;
        let dns_name = matches!(ServerName::try_from(domain.as_str()), Ok(ServerName::DnsName(_)));
        // TLS-ALPN-01 cannot validate a wildcard, SNI arrives lowercase, and CAs take no root dot.
        let canonical = *domain == domain.to_ascii_lowercase() && !domain.ends_with('.');
        ensure!(
            dns_name && domain.contains('.') && !domain.contains('*') && canonical,
            "api.tls.acme.domain must be a lowercase DNS name such as api.example.com"
        );
        let directory = Url::parse(&self.directory_url).context("api.tls.acme.directory_url")?;
        ensure!(directory.scheme() == "https", "api.tls.acme.directory_url must be https");
        ensure!(
            self.contact.strip_prefix("mailto:").is_some_and(|address| address.contains('@')),
            "api.tls.acme.contact must be a mailto: URL"
        );
        ensure!(
            self.renew_before.is_none_or(|before| before >= MIN_RENEW_BEFORE),
            "api.tls.acme.renew_before must be at least 14 days"
        );
        if let Some(test_ca) = &self.test_ca {
            ensure!(tee == TeeProvider::Mock, "api.tls.acme.test_ca requires tee.provider = \"mock\"");
            test_ca.root_certificate()?;
        }
        Ok(())
    }
}

impl AcmeTestCa {
    /// The root, as DER.
    /// Error cases:
    /// - `root` is not exactly one PEM certificate
    pub fn root_certificate(&self) -> anyhow::Result<CertificateDer<'static>> {
        let mut certificates = CertificateDer::pem_slice_iter(self.root.as_bytes());
        match (certificates.next(), certificates.next()) {
            (Some(Ok(root)), None) => Ok(root),
            _ => bail!("api.tls.acme.test_ca.root must be one PEM certificate"),
        }
    }
}
