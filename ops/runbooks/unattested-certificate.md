# `unattested_certificate`

Use this runbook when `unattested_certificate` fires, or when the Certificate Transparency watcher
raises `attestation_unverified`, `ct_log_unreachable` or `ct_watch_stopped`. All four are Critical
alerts. Commands, configuration keys and their output call a running trading server a node.

`unattested_certificate` means that a public certificate for the API's domain has been logged, and
that its key is not a TLS key that any verified attestation of the trading server has named. Either
the trading server's own certificate arrived before the watcher learned the trading server's new
key, or someone other than the enclave obtained a certificate for the domain. **Treat it as the
second case until you have shown that it is the first.**

The alert comes from `engine ct-watch`, a separate process on a host independent of the trading
server's host, which holds none of the trading server's keys.
[How the watcher works](#how-the-watcher-works) describes what it checks. The steps use the shell
variables that [Variables](reference.md#variables) sets up.

## The alert's fields

| Field | Meaning |
| --- | --- |
| `log` | The watcher's name for the Certificate Transparency log. |
| `index` | The entry's position in that log. |
| `issuer` | The certificate's issuer. |
| `key_sha256` | The SHA-256 of the certificate's SubjectPublicKeyInfo, in hexadecimal. It is the same value as the attestation's `tls_public_key_sha256` without its `0x` prefix. |

## 1. Is the watcher itself healthy?

Use this step first, whenever `unattested_certificate` fires, and on its own when
`attestation_unverified`, `ct_log_unreachable` or `ct_watch_stopped` fires. A watcher that cannot
verify the trading server's attestation cannot tell a restarted trading server's new key from a
foreign one.

1. If `attestation_unverified` is firing too, fix it first. The usual cause is an upgrade of the
   trading server while the watcher's `node.release` still names the old release's manifest. Point
   `node.release` at the new `release.toml`, and restart the watcher, as step 6 of
   [Upgrade to a new release](deploy-and-rollback.md#upgrade-to-a-new-release) describes.

   The other cause is an attestation of an enclave that the watcher's `node.enclaves` does not
   name: its log says `the attestation is of enclave 0x…, which node.enclaves does not name`. If
   that address is the vault's signer, after a promotion or a timelock `setSigner`, add it to
   `node.enclaves` and restart the watcher:

   ```shell
   cast call $VAULT 'checkpointSigner()(address)' --rpc-url $RPC
   ```

   An address that the vault does not name is another enclave answering the trading server's
   address, so treat any certificate for its key as foreign.

   **Changes the live system.** Restarting the watcher with a new manifest or a new enclave needs
   the owner's approval.

   If `unattested_certificate` then resolves by itself, this was its cause.

2. If `ct_log_unreachable` or `ct_watch_stopped` is firing, the watcher cannot see some or all of
   the logs. Neither alert explains `unattested_certificate`, but either means that other
   certificates may have gone unseen.

## 2. Is it the trading server's own key?

1. Read the trading server's current attested key:

   ```shell
   curl -sf $API/v1/attestation | jq -r '.tls_public_key_sha256[2:]'
   ```

2. Compare the hash with the alert's `key_sha256`, and follow the case that applies:

   - **The hashes are equal.** The certificate is the trading server's own, and the watcher has not
     yet verified an attestation that names this key. Check that the watcher's attestation fetches
     succeed, from `attestation_unverified` and from its log. Check also that the watcher's
     `attestation_interval` is at most half of its `grace`. The alert resolves at the next verified
     attestation, and nothing else needs to be done.
   - **The key is one that the trading server held before.** The trading server was given new keys,
     and a certificate for its old key is still valid. The watcher remembers a bounded set of keys,
     so it may have dropped the old one, or its state may have been reset. Compare the alert's
     `key_sha256` with the old attestation file that you kept, as
     [New keys](key-recovery.md#new-keys) describes. If the old attestation names the key, the
     certificate is the trading server's own, from before.
   - **The key is any other key.** The enclave did not ask for this certificate. Go on to
     [3. Establish what exists](#3-establish-what-exists).

## 3. Establish what exists

1. Set the API's domain, which is `api.tls.acme.domain` in the configuration:

   ```shell
   export DOMAIN=<domain>
   ```

2. List what has been issued for the domain, from a public index of the Certificate Transparency
   logs:

   ```shell
   curl -s "https://crt.sh/?q=$DOMAIN&output=json" | jq -r '.[] | [.id, .issuer_name, .not_before, .not_after, .serial_number] | @tsv'
   ```

   The index covers more logs than the watcher follows, so it may show certificates that the watcher
   has not seen.

3. Find the entry that matches the alert's issuer and time. Record its issuer, serial number and
   expiry. The owner needs them to ask the issuer to revoke the certificate.

4. Find out whether anyone is serving the certificate:

   ```shell
   openssl s_client -connect $DOMAIN:443 -servername $DOMAIN </dev/null 2>/dev/null \
     | openssl x509 -pubkey -noout | openssl pkey -pubin -outform der | sha256sum
   ```

   If the hash equals the attested key's hash, the foreign certificate is not in front of users on
   your network path. Run the same check from a second network too, because a network position or a
   DNS answer that you do not control can differ.

## 4. Contain

A certificate for another key lets someone with the right network position read or block a browser's
traffic to the domain, and serve a page that looks like the console. It does not let anyone sign as
the enclave. Signed answers, and the console's attestation check, verify against the enclave's
attested key, not against the certificate. A client that runs those checks is therefore not fooled,
but a client that trusts only the browser's padlock is.

1. Record the alert, the Certificate Transparency entry, the serial number, the issuer, and the time
   you saw it.

2. Tell the owner at once. Revocation, and what to tell users, are the owner's decisions.

3. Ask the issuing CA to revoke the certificate, through the CA's published channel for
   mis-issuance. Give the serial number, and the proof that the key is not the trading server's,
   which is the two hashes above. The owner sends this request.

4. Find out how the certificate was issued. With a TLS-ALPN-01 forward, whoever controls the DNS,
   the path to port 443, or the CA account can answer a validation. Check, in this order:

   - the DNS provider's change history, and the registrar account's recent logins;
   - whether the CAA record still names only the intended CA and method, and, by `accounturi`,
     only the enclaves' ACME accounts;
   - who can change the forward of port 443 on the host or the proxy, and its log;
   - the ACME account that obtained the certificate, from the CA's logs or the entry's issuer.

5. Close the hole that you found. Rotate the DNS and registrar credentials, restore the CAA record,
   or narrow the forward. Changing the hostname is a new release, as
   [Deploy and rollback](deploy-and-rollback.md) describes, and needs a new CAA record.

6. Silence the page in the alert receiver. The alert stays open in the watcher until the certificate
   expires, or until a verified attestation names its key.

   **Do not delete the watcher's state file to silence the page.** The file holds the watcher's
   position in each log. Without it, the watcher starts again from each log's current end, or from
   the log's `start_index` if one is set, and misses what was logged in between.

7. Leave the trading server alone. Nothing about this alert needs a restart, new keys or an
   administrative instruction.

**How to tell it is resolved.** The owner has the record and has asked the CA to revoke the
certificate, and the hole is closed. The watcher's alert ends when the certificate expires.

**When to escalate.** Tell the owner at step 2 of [4. Contain](#4-contain), whatever the cause turns
out to be.

## Prevent

- Run the watcher before the first certificate is issued, on a host that shares no failure with the
  trading server, and with every log that the CA submits to, as
  [Before the first certificate](certificate-renewal-failure.md#before-the-first-certificate)
  describes.
- Publish the CAA record with `validationmethods=tls-alpn-01`. Once the enclaves have registered
  their ACME accounts, replace it with one record for each account, each naming that account by
  `accounturi`, as
  [Before the first certificate](certificate-renewal-failure.md#before-the-first-certificate)
  describes.
- Point the watcher at a new release's manifest in the same change as the trading server's upgrade.

## How the watcher works

The watcher verifies the trading server's attestation every `attestation_interval`, and keeps every
TLS key hash that those attestations have named. It reads the Certificate Transparency logs in its
configuration. For a certificate that names the domain, or a wildcard that covers it, with a key not
in that set, it raises `unattested_certificate`. It waits `grace` after the entry's log time before
it raises the alert, which is 10 minutes by default. The alert repeats until an attestation names
the key or the certificate expires.
