# Key recovery

Which key is lost, replaced or exposed decides the procedure. Find it in the table, then follow its
section. Read [reference.md](reference.md) first.

| Key | Held by | Used for | If it is lost or exposed |
| --- | --- | --- | --- |
| Enclave signing key | The enclave only; sealed to the release on the data disk | Signs checkpoints and withdrawal authorizations; it is the vault's signer | "New keys" |
| API TLS key, ACME account, certificate | The enclave, sealed in the same file | The API's identity; the certificate names it | "New keys" |
| Operator admin key | The operator, offline. The node holds only the public key. | Signs `engine admin` commands | "Operator key" |
| Checkpoint relayer | The checkpoint RPC endpoint (`checkpoints.relayer`) | Pays gas for checkpoint posts; no authority over funds | "Relayer" |
| Governance multisig and guardian | The owner | Timelock proposals; the guardian's pause and signer retirement | Outside these runbooks |
| Webhook URLs, vendor API keys, RPC credentials | The operator | Alerts, prices, chain reads | "Secrets" |
| Access source authority | The screening service | Signs eligibility decisions; recorded in the log | "Secrets" |

## How the enclave key is kept

The keys are made inside the enclave and sealed to the release (launch measurement and config) with a
key the SEV-SNP firmware derives from the chip. No one can read them or export them, and there is no
backup. Two things move them: a restart (the same release on the same chip opens the file) and a
handover to the release the vault names next ([deploy-and-rollback.md](deploy-and-rollback.md)). If
neither is possible, the keys are gone and new ones must be made and registered by the timelock.

A restart needs no action. These need new keys:

- the sealed file is deleted, or the disk is lost;
- the VM moved to another chip, or the firmware was rolled back below the sealed level;
- the running release stopped after the vault named its successor, so its own copy no longer opens;
- the keys, or the host with the sealed file and the chip, are suspected compromised.

An edited config also stops the keys opening, but that is a new release, not a loss: hand the keys
to it ([deploy-and-rollback.md](deploy-and-rollback.md)). Check which case you are in before you
make new keys:

```sh
cast call $VAULT 'currentRelease()(bytes32)' --rpc-url $RPC       # what the vault names
cast call $VAULT 'releaseGeneration()(uint64)' --rpc-url $RPC
cast keccak $(cast concat-hex $DIGEST 0x$(sha256sum node.toml | cut -d' ' -f1))   # this disk's release
```

If the two hashes differ, the vault names another release: start that one, not new keys.

## New keys

New keys end signed withdrawals and checkpoints until the timelock installs them, and the timelock
has a delay (3 days, #360; `getMinDelay()`). Meanwhile no checkpoint is accepted and no withdrawal
authorization is valid. **Start the clock**: the vault's `exitPeriod` (14 days) counts from the last
accepted checkpoint, and after it anyone may start the permanent exit. If fewer than about five days
remain, the owner decides whether to start the install anyway or let the exit run.

1. **If the key may be exposed, stop it signing at once.** The guardian calls `retireSigner()`, which
   clears the vault's signer: no checkpoint and no withdrawal signed in the current epoch is accepted
   again, and it never expires. Add `pauseWithdrawals()` to stop claims of authorizations already
   issued (it lasts seven days and cannot be extended by the guardian; the timelock renews it with
   `setPauses`).
   **Changes the live system.** Needs the owner's approval, then the guardian multisig's signatures,
   without a delay.

   ```sh
   cast send $VAULT 'retireSigner()' --rpc-url $RPC --account <guardian>
   cast send $VAULT 'pauseWithdrawals()' --rpc-url $RPC --account <guardian>
   ```

   Check: `checkpointSigner()` prints `0x0000…0000`; `withdrawalsPaused()` prints `true`. The node
   raises `vault_withdrawals_halted` (cause `signer_retired` or `paused`) and `checkpoint_withheld`
   (cause `signer_retired`), and keeps sequencing. Make the markets reduce-only if they should take
   no new risk meanwhile ([reduce-only.md](reduce-only.md)).
2. **Get a place for the new keys.** There are two ways; both need the owner's approval to write keys.
   - *Same release, new generation (preferred).* The timelock calls `setCurrentRelease` with the
     release's own digest and config hash, which raises `releaseGeneration`. Nothing sealed in the old
     generation opens in the new one, so no old file stands in the way. Use the pattern in reference.md
     with `DATA=$(cast calldata 'setCurrentRelease(bytes,bytes32)' $DIGEST $CONFIG_HASH)`.
     **Changes the live system.** Needs the owner's approval of this contract call. This is not
     available while operator rotation is on; the operator's move to another approved release raises
     the generation instead ([operator-candidate.md](operator-candidate.md), section 5).
   - *Delete the old file.* With the VM stopped, remove `/data/keys/<release id>-<generation>.sealed.json`
     from the data disk. **Changes the live system.** Needs the owner's approval to delete keys. A
     host that deletes the file gets the same result as one that cannot read it, so only do this when
     the old keys are already unusable or retired.
3. **Boot with new keys.** Put an empty file `new-key` on the data disk, beside `config.toml`, and
   start the VM with the same image and the same config, except after the rotation-on move to another
   approved release: then the vault names that release, so boot its image and its config (the
   candidacy's release `B`) with the preserved log, since the old release's start refuses. The image removes the marker before it
   starts the engine, so it applies once. `--new-key` refuses to run beside an existing file for the
   same release and generation.
   **Changes the live system.** Needs the owner's approval to deploy.
   Check: `curl -sk $API/v1/attestation` answers; the console shows `signer_unregistered`; no other
   route answers. The new enclave address differs from the old `$SIGNER`.
4. **Verify the new attestation** and take the new address and pin
   ([reference.md](reference.md), "The node's attested key and its pin"). Do not skip this: the
   timelock registers whatever address you give it.
5. **Install it.** The timelock calls `setSigner(<new address>)`; pattern in reference.md.
   **Changes the live system.** Needs the owner's approval of this contract call. The vault accepts
   it from a retired state, and raises `signerEpoch`.

   ```sh
   DATA=$(cast calldata 'setSigner(address)' $NEW_SIGNER)
   ```

   While operator rotation is on, `setSigner` accepts only the enclave address the operator in effect
   registered: the candidate must first call `setEnclave($NEW_SIGNER)` on the registry
   ([operator-candidate.md](operator-candidate.md)). A different address reverts with
   `NotTheRegisteredEnclave`.
   Check: `checkpointSigner()` equals the new address and `signerEpoch()` rose by one; once the block
   is `finalized` the node starts serving by itself, and `signer_unregistered` and
   `vault_withdrawals_halted` resolve. `$PIN` is now the new TLS key's.
6. **Check the books carried over.** `latestSeq()` rises again. Authorizations that were still usable
   are reissued under the new epoch without a request; an expired one is reissued when its account
   asks for it. A pre-expiry authorization signed by the old key no longer pays. Replay across the
   change of signer needs an attestation for each signer:

   ```sh
   $ENGINE replay --log archive --attestation old.json --release old-release.toml --attestation new.json --release release.toml --processor <name>
   ```

   Keep the old attestation file: it is the only proof of what the old key was.
7. Update the companions that pin the key: ct-watch accepts the new TLS key from the next verified
   attestation; restart the benchmark worker with the new `$PIN`.
8. If the old key was exposed, say so to the owner, who decides what to publish. Every withdrawal the
   old key signed before the retirement is on the public log.

### The running release stopped

If the running release stopped after the vault named its successor and no successor confirmed, you
are here, with the alert `handover_failed`. The old release's sealed copy no longer opens. Follow
"New keys" from step 3: the vault already names a release, so the new keys seal for it.

## Operator key

The operator key signs admin commands. The node knows only its public key (`operator.public_key`);
it is recorded in the log, and changing it in the config for a log that has begun is refused.

- **Rotate with the key in hand** (rotation off). While rotation is on, `rotate-operator` is refused
  and the registry's own path for an operator to change its key is not built yet, so the key
  changes only by an election ([operator-candidate.md](operator-candidate.md)). With rotation off,
  and no election waiting to take effect, the old key signs `rotate-operator <new address>`.
  **Changes the live system.** Needs the owner's approval, and the new address must come from a key
  made on the owner's hardware.

  ```sh
  $ENGINE admin --config $CONFIG --node $API --pin $PIN rotate-operator 0x<new address>
  ```

  Check: `curl -sf $API/v1/operator | jq .pending_rotation` shows the address and an `effective_at`
  seven days on. The old key keeps working until then. From `effective_at`, sign with the new key and
  add `--rotated`:

  ```sh
  $ENGINE admin --config $CONFIG --node $API --pin $PIN --rotated --key-file new.key set-reduce-only false
  ```

  `operator.public_key` in the config can stay as it is: the node accepts the key a logged rotation
  names. A new release is not needed.
- **The key is exposed.** With rotation off, the same command is the only fix, and the exposed key
  keeps signing for seven days. With rotation on, only an election replaces it. Rotate at once, check the log for admin commands you did not send
  (`jq -c 'select(.entry=="command" and .cmd.type=="signed_admin") | {seq, ts, command: .cmd.command, events: [.events[].type]}' archive/0*.jsonl`
  lists every signed admin command in a downloaded archive), and consider making the markets
  reduce-only while the exposed key is still valid ([reduce-only.md](reduce-only.md)). Every admin command is public, and the
  protocol bounds the operator's limits (a profit fee or close-out level moves within 50% after a
  seven-day notice, and from protocol V55 the profit fee and its hold curve only up from launch).

  Rotating is not enough: until a release whose config names the new key runs, the exposed key can
  also seal a secrets bundle into the enclave. At every restart, before the log's rotation has been
  read, the delivery listener takes only the key the config names, and with rotation on it takes
  that key the whole time. A bundle the exposed key seals sets the webhook URLs the pages go to
  from the next restart on, and nothing bounds its version, so one sealed at the version's maximum
  refuses every later delivery on this release until a new release's own sealed file replaces it or
  the host deletes the file. **Ship a new release naming the new key**
  ([deploy-and-rollback.md](deploy-and-rollback.md), section 3): a config naming a key the log has
  not rotated to is refused at recovery, so the rotation must be logged before the new release can
  take the log; the new release takes the moved bundle only under the new key's signature, so
  deliver the bundle again under the new key. Until the new release runs, run `secrets-status`
  after every restart (the command is under "Secrets" below): the sealed version must be the newest
  you delivered, and any other version means the host is lying to you or the exposed key
  delivered — treat both as compromise.
- **The key is lost.** No other key can sign an admin command or a rotation while rotation is off.
  Keep two copies of the key in separate places. With rotation on, only an election replaces the
  operator ([operator-candidate.md](operator-candidate.md)), and a node holding for a benchmark
  snapshot the lost key would sign stays held: run the whole fallback in [standby.md](standby.md),
  which turns rotation off and names a release of its own. With rotation off, the governance
  fallback in [standby.md](standby.md) restores the node, and its `installOperator` step seats a
  new admin key the timelock alone can name.

## Relayer

`checkpoints.relayer` is an account on the checkpoint RPC endpoint, which signs and pays for the
transactions that post checkpoints. It has no authority over funds: only the enclave's signature
authorizes a checkpoint. If its balance runs out or the endpoint's key is lost, posts fail and
`checkpoint_post_failed` pages. There is no alert for a low balance: watch it from outside.

1. Fund the account, or stand up a new endpoint with a new funded relayer.
2. The relayer and endpoint are in the config: changing them is a new release
   ([deploy-and-rollback.md](deploy-and-rollback.md)). Funding the existing account is not.
3. Check: `checkpoint_post_failed` resolves and `latestSeq()` rises.

## Secrets

Webhook URLs, vendor API keys and RPC credentials. The config names them and the operator delivers
their values into the enclave; `crates/node/README.md`, "Delivered secrets", has the design and what a
host can do at each step. Keep the values in a bundle file on the admin workstation, never on the node's
host, with one entry for every name the config uses:

```toml
version = 1
[secrets]
provider_a_key = "..."
alerts_critical_url = "https://..."
```

`$SECRETS` is the listener's address as the host forwards it (`tee.sealing.secrets.listen`). The tool
checks the enclave's quote against `release.toml` and encrypts only after that. A real enclave takes
only the operator key in effect: the one the attested config names while it holds, and the one the log
records once it runs, or its own config's alone while an election that governs the key is in force —
one handing over, or whose key the log records — under the registry's rotation on, since an elected key
has no business in another operator's enclave. A rotated operator therefore keeps its old key until a release's config names the
new one, and an upgrade's new release takes a moved bundle only under the signature of the operator
its own config names; a bundle signed before a rotation is refused there, and the operator delivers
again. A fallback node whose config names the key the registry installed takes that key alone too,
from before the log mirrors the installation.

**See what an enclave holds.** This changes nothing. Check: the sealed version is the newest you
delivered. A lower one means the host restored an older sealed file: treat the host as suspect, and
deliver a higher version, which repairs it.

```sh
$ENGINE admin --config node.toml --key-file operator.key secrets-status --to $SECRETS --release release.toml
```

**Deliver.** **Changes the live system.** Needs the owner's approval of the delivery. A retry is safe:
the same version with the same secrets is accepted as already sealed, a lower version or the same
version with other secrets is refused, and no receipt means nothing is proven sealed.

```sh
$ENGINE admin --config node.toml --key-file operator.key deliver-secrets --to $SECRETS --release release.toml --secrets-file secrets.toml
```

Check: it prints `sealed: version <n>`. It refuses a bundle lacking a chain or screening key unless given
`--partial`, and names any integration that stays off. After an operator rotation the config still
names the old key, so signing under the new one takes `--rotated`, as for admin commands:
`$ENGINE admin --config node.toml --key-file operator.key --rotated deliver-secrets ...`.

**Rotate a secret.** Raise `version` and hold every secret, since a bundle replaces the whole sealed
one. List the new key at its provider first, and leave the old one valid: what runs keeps the clients
it started with.

1. Deliver. Check: it prints `restart required`, `secrets_restart_required` (Warning) fires, and the
   status shows the new sealed version and the old running one.
2. Restart the node ([restart-and-recovery.md](restart-and-recovery.md)); the enclave's keys stay.
   **Changes the live system.** Check: the alert resolves, and sealed and running versions match.
3. Revoke the old key at its issuer.

**If a secret is exposed**, treat it as lost now: revoke it at its issuer first (the vendor's console,
the RPC provider, the webhook receiver), then rotate as above.

- Revoking a vendor key stops that vendor's feed (`venue_feed_down`); a market with one vendor left
  keeps pricing only if its `min_sources` allows it. Make the markets that fall below it reduce-only.
- Revoking a custody provider's key takes that provider out of the quorum until the restart; with too
  few left, crediting pauses (`custody_rpc_quorum_lost`) until then.

**When a sealed file is lost or does not open**, the node raises `secrets_unreadable` and `secret_missing`
and, for a chain or screening key, holds at its next start, serving only the delivery listener. Deliver
again; it goes on starting with no restart.

**A new release** asks the running one for the bundle first, with no stop
([deploy-and-rollback.md](deploy-and-rollback.md), section 3). **An election moves nothing**: an elected
operator delivers its own to its own enclave ([operator-candidate.md](operator-candidate.md)).

- The access source's signing key is recorded in the log with its freshness bound. Replacing the
  key needs a fresh log; replacing its endpoint does not.
