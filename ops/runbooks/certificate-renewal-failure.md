# Certificate renewal failure

Use this runbook when the API's public certificate is missing or does not renew, when the sealed
certificate file does not open, and once before the first deployment. The alerts that bring you here
are `certificate_pending`, `certificate_expiring`, `sealed_certificates_unreadable`,
`sealed_certificate_write_failed` and `sealed_keys_write_failed`, and the Prometheus rules
`EngineTlsCertificateMissing` and `EngineTlsCertificateExpiring`. Commands, configuration keys and
their output call a running trading server a node.

The enclave obtains the API's certificate itself, from a public certificate authority (CA), for the
TLS key that the enclave generated and that its attestation names. While no certificate can be
issued or renewed, the API keeps serving, and clients that pin the attested key are unaffected.
Browsers, however, refuse the self-signed certificate that the trading server serves in the
meantime, and they refuse the API altogether once the last good certificate expires.

The steps use the shell variables that [Variables](reference.md#variables) sets up.
[How the trading server obtains its certificate](#how-the-trading-server-obtains-its-certificate)
describes the ordering and renewal behind the alerts.

## Diagnose

Use this procedure when `certificate_pending` or `certificate_expiring` fires, or when
`EngineTlsCertificateMissing` or `EngineTlsCertificateExpiring` fires in Prometheus.
`certificate_pending` is a Warning that no CA certificate has been issued yet.
`certificate_expiring` is a Warning that fewer than 14 days of the certificate's life remain and
renewal is failing. `EngineTlsCertificateMissing` fires when no certificate has been issued for 30
minutes, and `EngineTlsCertificateExpiring` fires within 7 days of expiry.

1. Set the API's domain, which is `api.tls.acme.domain` in the configuration:

   ```shell
   export DOMAIN=<domain>
   ```

2. Read the certificate's state from the metrics:

   ```shell
   curl -s http://<metrics host>:9464/metrics | grep -E '^engine_tls_certificate_(pending|not_after_seconds) '
   ```

   `<metrics host>` is the address of the trading server's metrics listener.
   `engine_tls_certificate_pending 1`, with `engine_tls_certificate_not_after_seconds 0`, means that
   no CA certificate has been issued yet. Otherwise `engine_tls_certificate_not_after_seconds` is
   the certificate's expiry, in Unix seconds.

3. Read the issuer and expiry of the certificate that the API serves:

   ```shell
   openssl s_client -connect $DOMAIN:443 -servername $DOMAIN </dev/null 2>/dev/null | openssl x509 -noout -issuer -enddate
   ```

   A CA's name as the issuer, with an expiry in the future, means the trading server holds a CA
   certificate.

4. Check that the served certificate is for the attested key. It must be, even when it is the
   self-signed certificate.

   ```shell
   openssl s_client -connect $DOMAIN:443 -servername $DOMAIN </dev/null 2>/dev/null \
     | openssl x509 -pubkey -noout | openssl pkey -pubin -outform der | sha256sum
   curl -sf $API/v1/attestation | jq -r '.tls_public_key_sha256[2:]'
   ```

   The two commands print the same hash. If they do not, something other than the enclave is
   answering, or a certificate for another key is being served. Go to
   [`unattested_certificate`](unattested-certificate.md).

5. Read the cause from the trading server's log. Each failed attempt logs a line of the form
   `no CA certificate yet, serving the self-signed one; retrying in …: <error>`. Read the error
   first.

6. Work down the causes in the table below, in order. Most failures have one of the first three
   causes.

   | Cause | Check | Fix |
   | --- | --- | --- |
   | The name does not reach the host. | `dig +short $DOMAIN` returns the address of the host, or of the proxy in front of it. | Fix the DNS records. If both an A and an AAAA record exist, both must reach the trading server, so remove a record that does not. |
   | Port 443 does not reach `node.listen` unmodified. | From outside, `openssl s_client -connect $DOMAIN:443 -servername $DOMAIN </dev/null` completes a handshake. A proxy must forward at layer 4 and never decrypt. If `[api.proxy_protocol]` is set, the proxy must send a PROXY protocol v2 header. | Fix the forward or the firewall. TLS must end in the enclave. |
   | A CAA record refuses the CA, the validation method or the account. | `dig +short CAA $DOMAIN` lists an `issue` entry for the CA in `directory_url`. If the entry names validation methods, it includes `validationmethods=tls-alpn-01`. If the entries name accounts by `accounturi`, one of them names the account in the log's latest `ACME account registered` or `using the sealed ACME account` line. | Publish `<domain>. CAA 0 issue "letsencrypt.org; validationmethods=tls-alpn-01"`, or the same record with the chosen CA's name. If the records name accounts, add a record for the missing account, as step 3 of [Before the first certificate](#before-the-first-certificate) describes. This is a DNS change, not a change to the trading server. |
   | The CA is rate-limiting this account or name. | The error says `rateLimited`. Let's Encrypt allows 5 certificates for the same set of names every 7 days, 10 new accounts for each IP address every 3 hours, and 5 failed authorisations for each name every hour. | Wait. Do not restart the trading server repeatedly, because without sealing every start registers a new account and places a new order. |
   | The CA is down or slow. | The CA's status page shows a problem, and the error is a timeout or a 5xx status. | Wait. The trading server retries by itself. |
   | The host clock is wrong. | The connection to the CA fails certificate verification, or an issued certificate is refused as expired or not yet valid. The trading server checks both against the host's clock, which the host controls. | Fix the host clock, and see `clock_drift` in [Alerts](alerts.md). |
   | The CA does not know the account. | The error says that the CA does not know the account. | Nothing. The trading server registers a new account by itself. |
   | The sealed certificate could not be written. | `sealed_certificate_write_failed` fired. The new certificate is served, but it is held in memory only. | Fix the data disk. Until then, a restart or the member's next turn places a new order, under the rate limits above. |
   | The sealed account could not be written. | `sealed_keys_write_failed` fired. The ACME account is held in memory only. | Fix the data disk. Until then, a restart registers a new account. |

7. After a fix, wait for the next retry, which is at most an hour away. To retry at once, restart
   the trading server as [Restart and recovery](restart-and-recovery.md) describes.

   **Changes the live system.** A restart needs the owner's approval. With `[tee.sealing]` set, a
   restart is safe, because it keeps the account and the certificate.

**How to tell it is resolved.** `certificate_pending` or `certificate_expiring` resolves, step 2
shows `engine_tls_certificate_pending 0` and a future expiry, and step 3 shows the CA as the issuer.

**When to escalate.** If renewal cannot be fixed before the certificate expires, follow
[If the certificate will expire first](#if-the-certificate-will-expire-first).

## The sealed certificate does not open

Use this procedure when `sealed_certificates_unreadable` fires. This Critical alert means the
trading server refused to start, because `<release id>.certificates.json` in `[tee.sealing].dir`
does not open for this release. The disk damaged the file, or the host altered it, or the host put
another release's file under its name. The trading server opens this file before the keys. A daily
turn's member that refuses therefore leaves the keys with the operator in effect.

The file holds only the certificate, which is public, and nothing that the trading server needs in
order to sign. Removing the file costs one new certificate order.

1. Check the data disk's health. If the host altered the file, treat it as an incident, as
   [Incident response](incident-response.md) describes.

2. Move `<release id>.certificates.json` aside, and keep it for the incident record.

   **Changes the live system.** The owner must approve moving the file and starting the trading
   server again.

   **Do not touch the keys' file, `<release id>-<generation>.sealed.json`.**

3. Start the trading server again, as [Restart and recovery](restart-and-recovery.md) describes.

**How to tell it is resolved.** The trading server starts and orders a new certificate, under the
CA's rate limits. [Diagnose](#diagnose) shows its progress.

## If the certificate will expire first

Use this procedure when renewal cannot be fixed before the current certificate expires. The current
certificate keeps serving until it expires.

1. Tell the owner the expiry, from `engine_tls_certificate_not_after_seconds`, the number of days
   remaining, and the cause.

2. The owner decides whether to move to another CA. The CA and the account contact are in the
   configuration, so moving is a new release, as [Deploy and rollback](deploy-and-rollback.md)
   describes. The new CA needs its own CAA record first.

**Once the certificate lapses, browsers refuse the API, so anything that relies on the public
certificate, such as the console, is unavailable.** A client that pins the attested key matches on
the key, not on the certificate's chain. Check whether your clients also check the certificate's
dates.

## Before the first certificate

Use this procedure once, before the first deployment that
[Deploy and rollback](deploy-and-rollback.md) describes.

1. Point the domain at the host, or at a pass-through proxy that never decrypts and that sends PROXY
   protocol v2 headers, listed in `[api.proxy_protocol]`. Forward port 443 unmodified, at layer 4,
   to `node.listen`. The CA validates the domain by connecting to port 443.

2. Publish a CAA record that names only the chosen CA and only the TLS-ALPN-01 method.

   The record narrows issuance to that CA and method. It does not narrow issuance to the enclave,
   because the host forwards port 443 and can answer a validation itself. What catches a certificate
   for another key is the Certificate Transparency watcher that
   [`unattested_certificate`](unattested-certificate.md) describes.

3. Once the enclaves have registered their ACME accounts, replace the record from step 2 with one
   record for each account, each naming that account by `accounturi`:

   ```
   <domain>. CAA 0 issue "letsencrypt.org; validationmethods=tls-alpn-01; accounturi=<account URL>"
   ```

   `<account URL>` is the `account` field of the trading server's `ACME account registered` and
   `using the sealed ACME account` log lines. The records then also bind issuance to those accounts.

   **A CA issues for the domain when any one of its `issue` records allows it. The record from
   step 2 names no account, so it allows every account until you remove it. An account that no
   record names cannot obtain a certificate.**

   Each member's hostname needs a record for each of these accounts:

   - The account sealed with the turn's keys. It moves with the keys at every handover and upgrade.
     Each member therefore orders under it for its own hostname during its turns.
   - With rotation on, the account sealed with each member's standby key. A promotion makes the
     promoted member's standby account the account of the turn's keys, which then moves to every
     member. Every member's hostname therefore needs a record for every member's standby account.

   A new standby key registers a new account. A new release seals new standby keys, and so does a
   member whose standby key was promoted or replaced. Add a record for each new account when the
   member's log reports it. Until then, the CA refuses the member's standby certificate, and the
   member retries by itself, at most an hour apart.

4. Start the Certificate Transparency watcher, `engine ct-watch`, on a host independent of the
   trading server's host.

   **Start the watcher before the first certificate is issued. The first time it reads a log, it
   starts at the log's current end, unless the log's `start_index` names an earlier entry, so it
   cannot see a certificate logged before it started.**

5. Rehearse against the CA's staging directory, which for Let's Encrypt is
   `https://acme-staging-v02.api.letsencrypt.org/directory`. Then change the configuration to the
   production directory.

   **Each directory is a release of its own, so rehearse on the testnet deployment, not on the
   launch deployment. Browsers do not trust a staging certificate.**

## How the trading server obtains its certificate

The certificate is validated over TLS-ALPN-01 on the API's own port, and renewed without a restart.

- **At boot.** The trading server orders a certificate when it starts. Until one arrives, it serves
  its self-signed certificate and logs `certificate_pending`.
- **With `[tee.sealing]`.** The ACME account is sealed with the keys, and moves with them. The
  certificate is sealed in the member's own file, `<release id>.certificates.json`, beside the keys,
  and no handover moves that file. A restart, or the member's next daily turn under the same TLS
  key, serves the certificate it holds, and orders nothing until renewal is due. A new release, such
  as an upgrade or a configuration change, orders its own certificate under the same account.
- **A member's standby key.** A rotation-set member that follows the operator also orders a
  certificate for its standby TLS key, which it would serve if promoted. It orders under an ACME
  account sealed with the standby keys. A restart orders under the same account, and a promotion
  makes it the account of the turn's keys.
- **Renewal.** The trading server renews once `renew_before` of the certificate's life remains. The
  configuration refuses a `renew_before` shorter than 14 days. When `renew_before` is unset, or is
  not shorter than the certificate's life, renewal starts with one third of the life remaining. A
  failed attempt keeps the current certificate, and the next attempt follows after a minute, with
  the wait doubling up to an hour. Once fewer than 14 days remain, each failure logs
  `certificate_expiring`.
- **Deadlines.** Every request to the CA has a 30-second deadline, and each attempt has 10 minutes.
  A lost answer, or a passing fault at the CA, takes up the same order again and costs no new
  issuance. A passing fault is `serverInternal`, `rateLimited`, or any 5xx or 429 status.
