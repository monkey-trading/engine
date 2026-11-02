# `unattested_certificate`

A public certificate for the API's domain has been logged whose key is not a TLS key any verified
attestation of the node has named. Either the node's own certificate arrived before the watcher
learned the node's new key, or somebody other than the enclave obtained a certificate for the
domain. Treat it as the second until you have shown it is the first. This is a Critical alert.

The alert comes from `engine ct-watch`, a separate process on a host independent of the node's. It
holds none of the node's keys. It verifies the node's attestation on an interval, keeps every TLS
key hash those attestations named, reads the Certificate Transparency logs it is configured with,
and raises the alert for a certificate that names the domain (or a wildcard that covers it) with a
key not in that set, `grace` (10 minutes by default) after the entry's log time. The alert repeats
until an attestation names the key or the certificate expires.

Read [reference.md](reference.md) first.

## The alert's fields

`log` (the watcher's name for the CT log), `index` (the entry's position), `issuer`, and
`key_sha256` (the SHA-256 of the certificate's SubjectPublicKeyInfo, the same value as the node's
`tls_public_key_sha256`).

## 1. Is the watcher itself healthy?

A watcher that cannot verify the node's attestation cannot tell a restarted node's new key from a
foreign one.

- If `attestation_unverified` is also firing, fix that first. The usual cause is a node upgrade
  with the watcher's `node.release` still pointing at the old release's manifest. Switch it to the
  new `release.toml` and restart the watcher ([deploy-and-rollback.md](deploy-and-rollback.md),
  section 3, step 6). If the alert then resolves on its own, it was this.
- `ct_log_unreachable` and `ct_watch_stopped` mean the watcher is blind to some or all logs. They
  do not explain this alert, but they mean other certificates may be unseen.

## 2. Is it the node's own key?

Fetch the node's current attested key and compare it with the alert:

```sh
curl -sf $API/v1/attestation | jq -r .tls_public_key_sha256
```

- **Equal to `key_sha256`:** the certificate is the node's. The watcher has not yet verified an
  attestation naming this key. Check that its attestation fetches succeed (`attestation_unverified`,
  its log), and that the watcher's `attestation_interval` is at most half its `grace`. The alert
  resolves at the next verified attestation. Nothing else to do.
- **A key the node held before:** the node was given new keys and a certificate for the old key is
  still valid. The watcher remembers a bounded set of keys, so it may have dropped the old one, or
  its state was reset. Confirm against the old attestation file you kept ([key-recovery.md](key-recovery.md),
  "New keys"); if the old key hash is there, the certificate is the node's own, from before.
- **Any other key:** go on. This is a certificate the enclave did not ask for.

## 3. Establish what exists

List what has been issued for the domain, from a public index of the CT logs (the watcher follows
only the logs in its config; this may show certificates it does not):

```sh
curl -s 'https://crt.sh/?q=api.example.com&output=json' | jq -r '.[] | [.id, .issuer_name, .not_before, .not_after, .serial_number] | @tsv'
```

Find the entry that matches the alert's issuer and time. Note its issuer, serial number and expiry;
the owner needs them to ask the issuer to revoke it. Then find out whether anyone is serving it:

```sh
openssl s_client -connect api.example.com:443 -servername api.example.com </dev/null 2>/dev/null \
  | openssl x509 -pubkey -noout | openssl pkey -pubin -outform der | sha256sum
```

If this equals the attested key hash, the foreign certificate is not in front of users from here.
Check from a second network too: a network position or DNS answer you do not control can differ.

## 4. Contain

What a certificate for another key can do: let someone with the right network position read or block
a browser's traffic to the domain, and serve a page that looks like the console. What it cannot do:
sign as the enclave. Signed answers and the console's attestation check verify against the enclave's
attested key, not against the certificate, so a client that runs them is not fooled. A client that
only trusts the padlock is.

1. **Record it.** Keep the alert, the CT entry, the serial, the issuer and the time you saw it.
2. **Tell the owner now.** Revocation and what to tell users are the owner's decisions.
3. **Ask the issuing CA to revoke** the certificate, through the CA's published channel for
   mis-issuance, with the serial number and the proof that the key is not the node's (the two hashes
   above). The owner sends this.
4. **Find how it was issued.** With a TLS-ALPN-01 forward, whoever controls the DNS, or the path to
   port 443, or the CA account, can answer a validation. Check, in this order:
   - the DNS provider's change history and the registrar account's recent logins;
   - the CAA record: does it still name only the intended CA and method (and the node's ACME
     account by `accounturi`)?
   - who can change the forward of port 443 on the host or the proxy, and its log;
   - the ACME account that obtained it (the CA's logs, or the entry's issuer).
5. **Close the hole** you found: rotate the DNS and registrar credentials, restore the CAA record,
   narrow the forward. Changing the hostname is a new release ([deploy-and-rollback.md](deploy-and-rollback.md))
   and a new CAA record.
6. **Do not delete the watcher's state file** to silence the page. It holds the positions in each
   log; without it the watcher restarts from each log's current end and misses what was logged
   meanwhile. Silence the page in the receiver. It stays open in the watcher until the certificate
   expires, or until a verified attestation names its key.
7. Do not touch the node. Nothing about this alert needs a restart, new keys or an admin command.

## Prevent

- Run the watcher before the first certificate is issued, on a host that shares no failure with the
  node, with every log the CA submits to ([certificate-renewal-failure.md](certificate-renewal-failure.md),
  "Before the first certificate").
- Publish the CAA record with `validationmethods=tls-alpn-01`, and, once the node has registered its
  ACME account, with `accounturi`.
- Switch the watcher to a new release's manifest in the same change as the node's upgrade.
