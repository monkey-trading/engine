# Certificate renewal failure

The API's browser-trusted certificate is ordered by the enclave itself, from a public CA, for the TLS
key the enclave generated and the attestation names. It is validated over TLS-ALPN-01 on the API's
own port, and renewed without a restart. If it cannot be issued or renewed, the API keeps serving
and clients that pin the attested key are unaffected, but browsers refuse a self-signed certificate
and, once the last good one expires, refuse the API.

Alerts: `certificate_pending` (Warning: no CA certificate yet), `certificate_expiring` (Warning:
fewer than 14 days remain and renewal is failing), and the Prometheus rules
`EngineTlsCertificateMissing` (pending for 30 minutes) and `EngineTlsCertificateExpiring` (within 7
days of expiry). Read [reference.md](reference.md) first.

## What the node does

- It orders a certificate at boot. Until one arrives it serves its self-signed certificate and
  logs `certificate_pending`. With `[tee.sealing]` the ACME account is sealed with the keys, and
  moves with them, and the certificate is sealed in the member's own file
  (`<release id>.certificates.json`, beside the keys), which no handover moves. A restart, or the
  member's next daily turn under the same TLS key, serves the certificate it holds and orders nothing
  until renewal. A new release (an upgrade or a config change) orders its own, under the same account.
- It renews once `renew_before` of the certificate's life remains (one third when unset, and never
  less than 14 days). A failed attempt keeps the current certificate and retries after a minute,
  doubling to an hour. Once fewer than 14 days remain, each failure logs `certificate_expiring`.
- Every request to the CA has a 30-second deadline and each attempt 10 minutes. A lost answer or a
  passing CA fault (`serverInternal`, `rateLimited`, any 5xx or 429) takes up the same order again
  and costs no new issuance.
- The log line of each failure carries the cause: `no CA certificate yet, serving the self-signed
  one; retrying in …: <error>`. Read the error first.

## Diagnose

1. Read the state:

   ```sh
   curl -s http://<metrics host>:9464/metrics | grep -E '^engine_tls_certificate_(pending|not_after_seconds) '
   openssl s_client -connect api.example.com:443 -servername api.example.com </dev/null 2>/dev/null | openssl x509 -noout -issuer -enddate
   ```

   `pending 1` with a `not_after_seconds` of `0` means no CA certificate yet. A CA issuer and a
   date in the future mean the node holds one; `not_after_seconds` is its expiry in Unix seconds.
2. Check the served certificate is for the attested key, which it must be even when it is the
   self-signed one:

   ```sh
   openssl s_client -connect api.example.com:443 -servername api.example.com </dev/null 2>/dev/null \
     | openssl x509 -pubkey -noout | openssl pkey -pubin -outform der | sha256sum
   curl -sf $API/v1/attestation | jq -r .tls_public_key_sha256
   ```

   The two hashes are equal. If they are not, you are not talking to the enclave, or a certificate
   for another key is being served: go to [unattested-certificate.md](unattested-certificate.md).
3. Work down the causes in order. Most failures are the first three.

| Cause | Check | Fix |
| --- | --- | --- |
| The name does not reach the host | `dig +short api.example.com` returns the host's (or the proxy's) address. | Fix DNS. Both A and AAAA must reach the node if both exist; remove a record that does not. |
| Port 443 does not reach `node.listen` unmodified | From outside: `openssl s_client -connect api.example.com:443 -servername api.example.com </dev/null` completes a handshake. A proxy must forward at layer 4 and never decrypt; if `[api.proxy_protocol]` is set it must send a PROXY protocol v2 header. | Fix the forward or the firewall. TLS must end in the enclave. |
| A CAA record refuses the CA or the method | `dig +short CAA api.example.com` lists an `issue` for the CA in `directory_url`, with `validationmethods=tls-alpn-01` if present. | Publish `api.example.com. CAA 0 issue "letsencrypt.org; validationmethods=tls-alpn-01"` (or the chosen CA's name). A DNS change, not a node change. |
| The CA is rate-limiting this account or name | The error says `rateLimited`. Let's Encrypt allows 5 certificates per name set every 7 days, 10 new accounts per IP every 3 hours and 5 failed authorizations per name per hour. | Wait. Do not restart in a loop: without sealing every start makes a new account and a new order. |
| The CA is down or slow | The CA's status page; the error is a timeout or a 5xx. | Wait; the node retries by itself. |
| The host clock is wrong | The connection to the CA fails certificate verification, or an issued certificate is refused as expired or not yet valid. The node checks them against the host's clock, which is untrusted. | Fix the host clock (and see `clock_drift` in [alerts.md](alerts.md)). |
| The account is unknown to the CA | The error says the CA does not know the account. | None: the node registers a new account by itself. |
| The sealed certificate could not be written | `sealed_certificate_write_failed` (Warning): the certificate is held in memory only. | Fix the data disk. Until then a restart or a next turn orders again, under the rate limits above. |
| The sealed account could not be written | `sealed_keys_write_failed` (Warning): the account is held in memory only. | Fix the data disk. Until then a restart registers a new account. |

4. After a fix, the next retry is at most an hour away. To retry now, restart the node
   ([restart-and-recovery.md](restart-and-recovery.md)); that is **a change to the live system** and needs
   the owner's approval. With sealing a restart is safe: it keeps the account and certificate.

## The sealed certificate does not open

`sealed_certificates_unreadable` (Critical): the node refused to start because
`<release id>.certificates.json` in `[tee.sealing].dir` does not open for this release: the disk
damaged it, or the host altered it or put another release's file under its name. Check the disk; a
file the host altered is an incident ([incident-response.md](incident-response.md)). The node opens
it before the keys, so a daily turn's member that refuses leaves the keys with the operator in effect.

The file holds only the certificate, which is public, and nothing the node needs to sign: removing
it costs one new order. With the owner's approval, move the file aside (keep it for the incident) and
start the node again; it orders a new certificate under the CA's rate limits. Do not touch the keys'
file, `<release id>-<generation>.sealed.json`.

## If the certificate will expire first

The current certificate keeps serving until it expires. If renewal cannot be fixed in time:

1. Tell the owner the expiry (`not_after_seconds`) and the cause now, with the remaining days.
2. The owner decides whether to move to another CA. The CA and the account contact are in the config,
   so that is a new release ([deploy-and-rollback.md](deploy-and-rollback.md)), and the new CA needs
   its own CAA record first.
3. Browsers need a valid certificate, so anything that relies on the public certificate (the
   console) is unavailable once it lapses. A client that pins the attested key matches on the key,
   not on the certificate's chain; check whether yours also checks the certificate's dates.

## Before the first certificate

Done once, before the first deployment ([deploy-and-rollback.md](deploy-and-rollback.md)):

1. Point the domain at the host, or at a pass-through proxy that never decrypts and sends PROXY
   protocol v2 headers (listed in `[api.proxy_protocol]`). Forward port 443 unmodified at layer 4 to
   `node.listen`: the CA validates by connecting to port 443.
2. Publish the CAA record naming only the chosen CA and only TLS-ALPN-01. The record narrows
   issuance to that CA and method; it does not narrow it to the enclave, because the host forwards
   port 443 and can answer a validation itself. What catches a certificate for another key is the
   watcher in [unattested-certificate.md](unattested-certificate.md). With sealing, a CAA
   `accounturi` naming the node's ACME account also binds issuance to it: the account's URL is in the
   node's log when it registers. Add that record after the first registration.
3. Start the Certificate Transparency watcher on a host independent of the node's, **before** the
   first certificate is issued: it reads each log from its current end the first time, so it cannot
   see a certificate logged before it started.
4. Rehearse against the CA's staging directory
   (`https://acme-staging-v02.api.letsencrypt.org/directory` for Let's Encrypt), then change the
   config to the production directory. Each is its own release, so rehearse on the testnet
   deployment, not on the launch deployment: a staging certificate is not trusted by browsers.
