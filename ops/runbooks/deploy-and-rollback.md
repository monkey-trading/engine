# Deploy and rollback

A release is a launch measurement (the image) plus a config file, and the vault's timelock names it
as the one release that may hold the enclave's keys. So three changes are the same procedure: a
first deployment, a new image, and a changed config. Even a one-line config change is a new release,
because the config's SHA-256 is part of the release's identity.

Read [reference.md](reference.md) first. Every step marked **Changes the live system** needs its own
approval; an approval for one step is not one for the next.

## Know before you start

- **The running release keeps sequencing until its successor has taken the keys.** Nothing stops
  trading while governance names the new release. Do not stop the running release to speed things up:
  once the vault names another release, its own sealed keys no longer open, and a stop loses them
  ([key-recovery.md](key-recovery.md)).
- **A new release may not be able to go back.** The node activates the newest protocol version when
  it starts. A log that has activated a version cannot be replayed by a build that predates it.
  Compare `ProtocolVersion::LATEST` in `crates/core/src/protocol.rs` between the running tag and the
  new one (`git diff <old tag> <new tag> -- crates/core/src/protocol.rs`). If it changed, the upgrade
  cannot be rolled back to the old image: fix forward.
- **While operator rotation is on, `setCurrentRelease` reverts** (`RotationActive`): the release the
  vault names is then the operator in effect's registered release, and the registry has no call that
  changes it. Run these procedures with rotation off. How an operator upgrades with rotation on is
  an open question for the owner ([operator-candidate.md](operator-candidate.md), "Open questions").
- **No staging environment exists yet.** Run this on a testnet vault first; the steps are the same.

## 1. Prepare a release

Nothing here touches a live system.

1. Tag `v<version>` on a `main` commit whose CI run passed. Take `vmlinuz`, `initramfs.cpio.gz`,
   `OVMF.fd`, `measurement.json` and `SHA256SUMS` from that run's `image-first` artifact, or, once
   the artifact has expired, from `gh workflow run ci.yaml --ref v<version>`.
2. On a second machine, rebuild: `scripts/build-image.sh --ref v<version> --no-cache <dir>`. Check:
   `diff <dir>/SHA256SUMS <ci>/SHA256SUMS` prints nothing. Stop if it differs.
3. Assemble the config from the template, the benchmark publishers and the production genesis, and
   fill in every value of [production-node.toml](production-node.toml) that carries an `OWNER` or
   `REPLACE` marker, deleting the marker comment as you set it:

   ```sh
   cat ops/runbooks/production-node.toml config/benchmark-publishers.toml config/production-genesis.toml > node.toml
   grep -nE 'OWNER|REPLACE' node.toml        # must print nothing
   ```

   Check that it loads. `engine admin` reads the config before it asks for a key, so this fails with
   the config's error, or reaches the key check when the config is good:

   ```sh
   env -u OPERATOR_KEY -u OPERATOR_KEY_FILE $ENGINE admin --config node.toml --pin $(printf '0%.0s' $(seq 64)) set-reduce-only false
   ```

   Check: the last line is `Error: no operator key: pass --key-file, or set OPERATOR_KEY_FILE or OPERATOR_KEY`.
   A first line `Error: parsing <file>` or `Error: validating <file>`, followed by `Caused by:`,
   names the key to fix.
4. Hash it: `sha256sum node.toml`. This is `$CONFIG_HASH`, the second argument of `setCurrentRelease`.
   Do not edit the file again: one changed byte is another release.
5. Write `release.toml` with the release name, `measurement.json`'s `launch_digest` and `policy`, the
   minimum reported TCB, and `[config_hashes]` holding `"node.toml" = "0x<hash>"` (format in
   `image/README.md`, "Release"). The minimum TCB is `OWNER`: take it from the host's firmware.
6. Publish the GitHub release with the image files, `node.toml`, `release.toml` and the browser
   verifier package. The config holds no credential, only the names of the secrets the operator
   delivers.
7. Ship a console build that pins `release.toml`, next to the previous release's, before the node
   release goes live. The attestation binds the config, so a console that does not pin the new
   manifest refuses the new node.
8. If the previous release is running: copy the log off it now (reference.md, "Download the public
   log"). The successor needs the log to start from.

## 2. First deployment

Prerequisites, each done and checked before step 1 of this list: the contracts deployed and wired
(`contracts/README.md`, "Governance and deployment"), with the vault's signer a placeholder and its
`registry` and the registry's `vault` set; the host (bare-metal AMD SEV-SNP); DNS for the API
domain and a CAA record ([certificate-renewal-failure.md](certificate-renewal-failure.md), "Before
the first certificate"); a Critical alert receiver; and the companion processes in section 5 planned.

1. **Name the release.** The timelock calls `setCurrentRelease(launch_digest, config_hash)` on the
   vault. Use the pattern in reference.md, with:

   ```sh
   DIGEST=0x...                                   # measurement.json: launch_digest (48 bytes)
   CONFIG_HASH=0x$(sha256sum node.toml | cut -d' ' -f1)
   TARGET=$VAULT
   DATA=$(cast calldata 'setCurrentRelease(bytes,bytes32)' $DIGEST $CONFIG_HASH)
   ```

   **Changes the live system.** Needs the owner's approval of this contract call, then the multisig's
   signatures on the schedule and the execute.
   Check: `cast call $VAULT 'currentRelease()(bytes32)' --rpc-url $RPC` equals
   `cast keccak $(cast concat-hex $DIGEST $CONFIG_HASH)`, and `releaseGeneration()` rose by one.
   Wait until the block holding the execute is `finalized` (about 15 minutes after its batch posts;
   see reference.md): the node reads the release there.
2. **Boot with new keys and deliver the secrets.** Make the data disk with `config.toml` (the file you
   hashed, renamed) and an empty file `new-key`. Start the VM on the host with the launch flags in
   `measurement.json` (`image/README.md`, "Boot on an SEV-SNP host"), forwarding
   `tee.sealing.secrets.listen` from the operator's addresses.
   **Changes the live system.** Needs the owner's approval to deploy this image and config.
   The node holds, with `secret_missing` for each secret and only its delivery listener up: chain
   access is what lets it go on. Deliver the bundle ([key-recovery.md](key-recovery.md), "Secrets").
   **Changes the live system.** Needs the owner's approval of the delivery.
   Check: the tool prints `sealed: version 1`, `curl -sk $API/v1/attestation` answers and `/v1/markets`
   does not. The serial console shows `signer_unregistered`. That is expected: the vault does not name
   this key yet. If it shows `secrets_start_failed` instead, a provider refused a delivered key: see
   [alerts.md](alerts.md).
3. **Verify the attestation**, as in reference.md ("The node's attested key and its pin"):
   `verify-attestation` against `release.toml`. Check that it exits 0 and that the config hash it
   names is `$CONFIG_HASH`.
4. **Register the signer.** The timelock calls `setSigner($SIGNER)`:

   ```sh
   TARGET=$VAULT
   DATA=$(cast calldata 'setSigner(address)' $SIGNER)
   ```

   **Changes the live system.** Needs the owner's approval of this contract call and the
   multisig's signatures. `setSigner` raises `signerEpoch`.
   Check: `cast call $VAULT 'checkpointSigner()(address)' --rpc-url $RPC` equals `$SIGNER`. Once
   the block is `finalized` the node starts on its own: it reads the vault every
   `checkpoints.retry_interval`, so `signer_unregistered` resolves within seconds of that, and
   `/v1/markets` answers once the node has recovered its log.
5. **First boot work happens by itself.** On its first trusted time sample (Roughtime) the node
   activates the newest protocol version and sequences `config/production-genesis.toml` as admin
   commands. Check: `curl -sf $API/v1/markets | jq length` prints the number of launch markets, and
   `curl -sf $API/v1/house/state | jq .seq` rises. A `genesis_refused` or
   `protocol_activation_failed` page means the config's genesis does not apply to a log: do not
   edit the log; fix the config and make a new release.
6. **Check the certificate.** `curl -sf $API/v1/health` succeeds without `-k`, and
   `curl -s http://<metrics host>:9464/metrics | grep engine_tls_certificate_pending` prints `0`.
   If not, go to [certificate-renewal-failure.md](certificate-renewal-failure.md).
7. **Check checkpoints and withdrawals.** After a few minutes of trading or a test deposit:
   `cast call $VAULT 'latestSeq()(uint64)'` rises, and `engine_withdrawals_paused` is `0`.
   `engine_withdrawals_paused 1` before the first reconciliation is normal; see
   [withdrawal-stalls.md](withdrawal-stalls.md) if it stays.
8. **One-time initialisation**, each only once per log, with values the owner has decided (#436):

   ```sh
   $ENGINE admin --config node.toml --node $API --pin $PIN init-emissions --start <RFC 3339 time> --period-budget <base units>
   $ENGINE admin --config node.toml --node $API --pin $PIN init-staking --start <RFC 3339 time> --liquidity-floor <USD> --epoch-ms <ms> --cooldown-ms <ms>
   ```

   **Changes the live system.** Each needs the owner's approval of the exact values. The period
   budget must equal the one the vault's MONKEY cap was derived from; the node refuses another
   (`emission_budget_mismatch`). The staking cooldown and epoch must equal the staking contract's.
9. **House capital** enters only by a vault deposit to the house (`depositToHouse`). **Changes the
   live system.** Needs the owner's approval of the amount. The production genesis sets no house cap
   floors: until the owner sets them, no opening trade is admitted. That is the owner's decision
   before launch, not a fault.

## 3. Upgrade to a new release

Use this for a new image, a new config, or both. The running release is `A`, the new one `B`.

1. Prepare `B` (section 1). Check that the protocol-version note in "Know before you start" is
   settled, so you know whether this can be undone.
2. **Name `B`.** Section 2, step 1, with `B`'s digest and config hash.
   **Changes the live system.** Needs the owner's approval of this contract call.
   Check: `A` raises `release_superseded` (Critical) within a minute of the block being `finalized`,
   and keeps sequencing. This page is expected; it says a restart of `A` would now find no keys.
3. **Prepare `B`'s disk.** On the new host or disk: `config.toml` (`B`'s config), a file
   `predecessor` holding `A`'s handover address, `host:port` (`tee.sealing.handover.listen`), and
   the log you copied at step 8 of section 1, in `/data/log`. Check: `A`'s handover port is
   reachable from `B`'s network, and `B`'s `[tee.sealing.handover]` and `[tee.sealing.secrets]` are
   configured the same way.
4. **Start `B`.**
   **Changes the live system.** Needs the owner's approval to deploy this image and config and to
   hand the enclave's keys to it.
   What happens: `B` first asks `A` for the delivered secrets, with no stop: `A` checks `B`'s quote
   against the vendor's chain and the vault's named release, and sends the bundle encrypted to `B`,
   which takes it only under the signature of the operator `B`'s config names. If the move fails, `B`
   raises `secret_missing`: deliver to `B` yourself ([key-recovery.md](key-recovery.md), "Secrets"): `A` refuses
   while rotation is on, and a bundle signed before an operator rotation does not move.
   `B` then connects to `A` for the keys. `A` checks `B`'s quote in the same way, stops sequencing, signs a final checkpoint, and sends the keys and the
   log tail encrypted to `B`. `B` writes the log, checks it reaches that checkpoint, seals the keys
   and confirms; `A` acknowledges and powers off.
   Check: `B` serves `/v1/markets`; `secrets-status` on `B` shows the version `A` held;
   `curl -sf $API/v1/attestation | jq -r .enclave_address` still
   equals `$SIGNER`; `A`'s VM has powered off; `release_superseded` resolves; and the chain of
   posted checkpoints continues: `cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC` rises.
   The TLS key and certificate carry over, so `$PIN` is unchanged.
5. **If a page says the handover did not finish**, do not stop anything.
   - `handover_unconfirmed` on `A`: sequencing has stopped and no successor holds the keys yet. `A`
     keeps serving successors; start `B` again. Stopping `A` loses its keys.
   - `handover_unconfirmed` on `B`: the keys are sealed, but `A` did not acknowledge, so it may
     still run. Wait until `A` has powered off, then start `B` again.
   - `handover_failed` on `A`: it stopped without a successor, and its keys no longer open. This is
     [key-recovery.md](key-recovery.md), "The running release stopped".
   - A refused `B` start naming a log that does not reach the checkpoint: copy the log again
     (section 1, step 8) and start `B` again.
6. **Update the companions** (section 5): point ct-watch at `B`'s `release.toml` straight away,
   or it raises `attestation_unverified`. Re-pin the benchmark worker only if the pin changed.
7. **Keep `A`'s disk until `B` has run one full day**, then wipe it. `A`'s sealed copy can never
   open again, and holds nothing `B` needs.

## 4. Roll back

Choose by where you are.

**`A` still running; `B` has not taken the keys.** Call the upgrade off.
1. The timelock calls `setCurrentRelease(A's digest, A's config hash)` again (section 2, step 1).
   **Changes the live system.** Needs the owner's approval of this contract call.
   This starts another generation, so `A` keeps raising `release_superseded`: its sealed keys are
   from the earlier generation.
2. To make `A` restart-safe again, start a fresh copy of `A`'s image and config with the
   `predecessor` file pointing at the running `A`, as in section 3, step 4.
   **Changes the live system.** Needs the owner's approval.
   Check: the fresh copy serves, the old VM powers off, and `release_superseded` resolves.

**`B` has the keys and is running.** This is another upgrade, to `A`'s release.
1. Check the protocol version rule in "Know before you start". If the new release activated a newer
   version, there is no rollback; ship a fix as `C`.
2. Run section 3 with `A`'s image and config as the new release. The vault names it in a new
   generation, so `A` takes the keys afresh from `B`.

**`B` is the problem because of a config value** (a feed, a limit, a provider): that is also a new
release. There is no way to change a running node's config. A secret's value is not a config value:
a rotation needs a delivery and a restart, and no release ([key-recovery.md](key-recovery.md), "Secrets").

## 5. Companion processes

These run outside the enclave. None holds the enclave's keys.

- **Benchmark worker.** With `[benchmarks]` in the config, a node waits for a signed publisher
  snapshot at every start before it sequences ordinary commands. Run, on a host of its own, with
  `OPERATOR_KEY_FILE` set for this process alone:

  ```sh
  $ENGINE admin --config node.toml --node $API --pin $PIN sync-benchmarks --watch
  ```

  Start it before the node does, or as soon as the node serves its attestation: it retries until
  the node answers. Restart it after every node restart that changes the pin. Alerts:
  `benchmark_sync_failed`, `benchmark_fetch_failed`, `benchmark_sync_pending`,
  `benchmark_rate_refused`. It posts its own alerts through the config's `[alerts]` webhooks, so set
  their environment variables here too, and it keeps its own outbox beside the node's, in the outbox
  directory's name with `-benchmark-worker` added.
- **Certificate Transparency watcher**, on a host independent of the node's:
  `$ENGINE ct-watch --config ct-watch.toml` (template: `config/ct-watch.toml`). Start it before the
  first certificate is issued. See [unattested-certificate.md](unattested-certificate.md).
- **Prometheus** scraping the node's metrics listener and the watcher's, with
  `ops/prometheus/engine-node-rules.yml`, routed to a receiver other than the node's own webhook, and
  the Grafana dashboard `ops/grafana/engine-node.json`.
- **Archive mirrors.** Two stores that do not share a failure, each fed by the download loop in
  reference.md. A standby and a successor operator read the log from them when the node is down.

## Staging

Run sections 2 to 4 against a testnet vault and a second host, with the same image and a config that
differs only in its chain, vault and providers. Both procedures are done when they complete without
a step that is not written here.
