# Key recovery

Use this runbook when a key is lost, must be replaced, or may be exposed, and to deliver or rotate
the secrets that the trading server reads. Which key is affected decides the procedure. Find the
key in the table below, and follow its section. The steps use the shell variables that
[Variables](reference.md#variables) sets up. Commands and their output call a running trading server
a node.

- [How the enclave key is kept](#how-the-enclave-key-is-kept)
- [New keys](#new-keys)
- [Operator key](#operator-key)
- [Relayer](#relayer)
- [Secrets](#secrets)

| Key | Held by | Used for | If it is lost or exposed |
| --- | --- | --- | --- |
| Enclave signing key | The enclave only, sealed to the release on the data disk | Signs checkpoints and withdrawal authorisations. It is the vault's signer. | [New keys](#new-keys) |
| API TLS key and ACME account | The enclave, sealed in the same file as the signing key | The API's identity, and the account under which its certificates are ordered | [New keys](#new-keys) |
| API certificate | The enclave, sealed apart from the keys in the member's own file, which no handover moves | Names the member's API hostname and the TLS key. It is public. | The trading server orders another, as [Certificate renewal failure](certificate-renewal-failure.md) describes. |
| Operator admin key | The operator, offline. The trading server holds only the public key. | Signs `engine admin` instructions | [Operator key](#operator-key) |
| Checkpoint relayer | The checkpoint RPC endpoint, as `checkpoints.relayer` | Pays the gas for checkpoint posts. It has no authority over funds. | [Relayer](#relayer) |
| Governance multisig and guardian | The owner | Timelock proposals, and the guardian's pause and signer retirement | Outside these runbooks |
| Webhook URLs, vendor API keys and RPC credentials | The operator | Alerts, prices and chain reads | [Secrets](#secrets) |
| Access source authority | The screening service | Signs eligibility decisions. The log records it. | [Secrets](#secrets) |

## How the enclave key is kept

Use this section when `enclave_key_failed` fires, or before you decide to make new keys, to find out
whether the keys are lost.

The enclave makes its keys itself, and seals them to the release, which is the launch measurement
and the configuration, with a key that the SEV-SNP firmware derives from the chip. No one can read
or export the keys, and there is no backup. Only two things carry the keys forward:

- a restart, in which the same release on the same chip opens the sealed file;
- a handover to the release that the vault names next, as
  [Deploy and rollback](deploy-and-rollback.md) describes.

If neither is possible, the keys are gone. New keys must then be made, and the timelock must
register them. A restart needs no action. These cases need new keys:

- the sealed file is deleted, or the data disk is lost;
- the VM moved to another chip, or the firmware was rolled back below the level the keys were
  sealed at;
- the running release stopped after the vault named its successor, so the release's own copy no
  longer opens;
- the keys, or the host holding both the sealed file and the chip, may be compromised.

An edited configuration also stops the keys from opening. That case is a new release, not a loss:
hand the keys to it, as [Deploy and rollback](deploy-and-rollback.md) describes.

1. Before you make new keys, read the release that the vault names, and its generation:

   ```shell
   cast call $VAULT 'currentRelease()(bytes32)' --rpc-url $RPC
   cast call $VAULT 'releaseGeneration()(uint64)' --rpc-url $RPC
   ```

2. Compute the release on this data disk, where `$DIGEST` is the release's launch digest from its
   `measurement.json`:

   ```shell
   cast keccak $(cast concat-hex $DIGEST 0x$(sha256sum node.toml | cut -d' ' -f1))
   ```

   If this hash differs from `currentRelease()`, the vault names another release. Start that
   release, and do not make new keys.

## New keys

Use this procedure when the enclave's keys are lost or may be exposed, as
[How the enclave key is kept](#how-the-enclave-key-is-kept) describes, or when `handover_failed`
fires.

New keys stop signed withdrawals and checkpoints until the timelock installs them. The timelock has
a delay, three days at launch, which `cast call $TIMELOCK 'getMinDelay()(uint256)' --rpc-url $RPC`
reads. Meanwhile, the vault accepts no checkpoint, and no withdrawal authorisation is valid.

**Start the clock now.** The vault's `exitPeriod`, 14 days, counts from the last accepted
checkpoint, and once it has passed, anyone may start the permanent exit. If fewer than about five
days remain, the owner decides whether to start the installation anyway or to let the exit run.

1. **If the key may be exposed, retire the signer at once.** The guardian calls `retireSigner()`,
   which clears the vault's signer. No checkpoint, and no withdrawal signed in the current epoch, is
   then accepted again, and the retirement never expires.

   **Changes the live system.** The owner must approve the call, and the guardian multisig signs it,
   without a delay.

   ```shell
   cast send $VAULT 'retireSigner()' --rpc-url $RPC --account <guardian>
   ```

   `<guardian>` is the guardian's account. The guardian is the multisig, so submit the call as a
   multisig transaction. Afterwards, `cast call $VAULT 'checkpointSigner()(address)' --rpc-url $RPC`
   prints `0x0000000000000000000000000000000000000000`.

2. **If the key may be exposed, pause withdrawals.** `pauseWithdrawals()` stops the claims of
   authorisations already issued. The pause lasts seven days, and the guardian cannot extend it. The
   timelock renews it with `setPauses`.

   **Changes the live system.** The owner must approve the call, and the guardian multisig signs it,
   without a delay.

   ```shell
   cast send $VAULT 'pauseWithdrawals()' --rpc-url $RPC --account <guardian>
   ```

   Afterwards, `cast call $VAULT 'withdrawalsPaused()(bool)' --rpc-url $RPC` prints `true`. The
   trading server raises `vault_withdrawals_halted`, with the cause `signer_retired` or `paused`,
   and `checkpoint_withheld`, with the cause `signer_retired`, and it keeps sequencing. If the
   markets should take no new risk meanwhile, make them reduce-only, as
   [Make markets reduce-only](reduce-only.md) describes.

3. **Make room for the new keys,** in one of two ways. Both need the owner's approval to write keys.

   - **Same release, new generation.** This is the preferred way. The timelock calls
     `setCurrentRelease` with the release's own digest and configuration hash, which raises
     `releaseGeneration`. Nothing sealed in the old generation opens in the new one, so no old file
     stands in the way. Follow
     [Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock)
     with:

     ```shell
     DATA=$(cast calldata 'setCurrentRelease(bytes,bytes32)' $DIGEST $CONFIG_HASH)
     ```

     **Changes the live system.** The owner must approve this contract call. This way is not
     available while operator rotation is on. The operator's move to another approved release raises
     the generation instead, as
     [Upgrade while you are the operator](operator-candidate.md#upgrade-while-you-are-the-operator)
     describes.
   - **Delete the old file.** With the VM stopped, remove
     `/data/keys/<release id>-<generation>.sealed.json` from the data disk.

     **Changes the live system.** The owner must approve deleting keys. **A host that deletes the
     file gets the same result as one that cannot read it, so do this only when the old keys are
     already unusable or retired.**

4. **Boot with new keys.** Put an empty file named `new-key` on the data disk, beside `config.toml`,
   and start the VM with the same image and the same configuration. The image removes the marker
   before it starts the engine, so the marker applies once. `--new-key` refuses to run beside an
   existing sealed file for the same release and generation.

   After a rotation-on move to another approved release, the vault names that release instead.
   Boot that release's image and configuration, the candidacy's release `B`, with the preserved log.
   The old release's start opens none of its keys, because with rotation on it only follows the
   operator in effect.

   **Changes the live system.** The owner must approve the deployment.

   `curl -sk $API/v1/attestation` answers, the console shows `signer_unregistered`, and no other
   route answers. The new enclave address differs from the old `$SIGNER`.

5. **Verify the new attestation,** and take the new address and pin from it, as
   [The trading server's attested key and its pin](reference.md#the-trading-servers-attested-key-and-its-pin)
   describes. **Do not skip this step. The timelock registers whatever address you give it.**

6. **Install the new signer.** The timelock calls `setSigner` with the new address, following
   [Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock)
   with:

   ```shell
   DATA=$(cast calldata 'setSigner(address)' $NEW_SIGNER)
   ```

   **Changes the live system.** The owner must approve this contract call. The vault accepts it from
   a retired state, and raises `signerEpoch`.

   **While operator rotation is on, `setSigner` accepts only the enclave address that the operator
   in effect registered. Any other address reverts with `NotTheRegisteredEnclave`.** The candidate
   must first register the new address on the registry:

   ```shell
   cast send $REGISTRY 'setEnclave(address)' $NEW_SIGNER --rpc-url $RPC --account <candidate>
   ```

   `<candidate>` is the candidate's account, as
   [Standing as an operator candidate](operator-candidate.md) describes.

   `cast call $VAULT 'checkpointSigner()(address)' --rpc-url $RPC` prints the new address, and
   `signerEpoch()` has risen by one. Once the block is `finalized`, the trading server starts
   serving by itself, and `signer_unregistered` and `vault_withdrawals_halted` resolve. `$PIN` is
   now the new TLS key's hash.

7. **Check that the books carried over.** `latestSeq()` rises again. Authorisations that were still
   usable are reissued under the new epoch without a request. An expired authorisation is reissued
   when its account asks for it. An authorisation that the old key signed before it expired no
   longer pays. A replay across the change of signer needs an attestation for each signer:

   ```shell
   $ENGINE replay --log archive --attestation old.json --release old-release.toml \
     --attestation new.json --release release.toml --processor <name>
   ```

   `<name>` is the product under which AMD's key distribution service issues the host's
   certificates: `Milan`, or `Genoa` for a Genoa, Bergamo or Siena host. The replay
   prints a line that begins `verified:`. **Keep the old attestation file. It is the only proof of
   what the old key was.**

8. **Update the companion processes that pin the key.** The Certificate Transparency watcher accepts
   the new TLS key from the next verified attestation. Restart the benchmark worker with the new
   `$PIN`, as [Companion processes](deploy-and-rollback.md#companion-processes) describes.

9. **If the old key was exposed, tell the owner,** who decides what to publish. Every withdrawal
   that the old key signed before the retirement is on the public log.

**How to tell it is resolved.** `signer_unregistered` and `vault_withdrawals_halted` have resolved,
`latestSeq()` rises, and a replay across the change of signer ends with `verified:`.

**When to escalate.** Tell the owner as soon as new keys are needed, with the time of the last
accepted checkpoint, because the exit period is running.

### The running release stopped

Use this procedure when `handover_failed` fires. The running release stopped after the vault named
its successor, and no successor confirmed. The old release's sealed copy no longer opens. Follow
[New keys](#new-keys) from step 4, which boots with new keys. The vault already names a release, so
the new keys are sealed for it.

## Operator key

The operator key signs administrative instructions. The trading server knows only its public key,
`operator.public_key`. The log records that key, and the trading server refuses a configuration that
changes it for a log that has already begun.

### Rotate the operator key

Use this procedure to replace the operator key while you still hold it, with operator rotation off.

While rotation is on, the engine refuses `rotate-operator`, and the registry has no function that
changes an operator's admin key. The key then changes only through an election, as
[Standing as an operator candidate](operator-candidate.md) describes. With rotation off, and with no
election waiting to take effect, the old key signs the rotation.

1. Sign the rotation with the old key, where `<new address>` is the new key's address.

   **Changes the live system.** The owner must approve it, and the new address must come from a key
   made on the owner's hardware.

   ```shell
   $ENGINE admin --config $CONFIG --node $API --pin $PIN rotate-operator 0x<new address>
   ```

2. Confirm the pending rotation:

   ```shell
   curl -sf $API/v1/operator | jq .pending_rotation
   ```

   The output shows the new address and an `effective_at` seven days later. The old key keeps
   working until then.

3. From `effective_at`, sign with the new key and add `--rotated`, for example:

   ```shell
   $ENGINE admin --config $CONFIG --node $API --pin $PIN --rotated --key-file new.key \
     set-reduce-only false
   ```

`operator.public_key` in the configuration can stay as it is, because the trading server accepts the
key that a logged rotation names. No new release is needed.

### The operator key is exposed

Use this procedure when the operator key may be known to someone else.

With rotation off, the rotation above is the only fix, and the exposed key keeps signing for seven
days. With rotation on, only an election replaces the key. Every administrative instruction is
public, and the protocol bounds what the operator can do. A profit fee or close-out level can move
only within 50% of its launch value, after a seven-day notice, and the profit fee and its holding
curve can only rise from their launch values.

Rotating the key is not enough. Until a release whose configuration names the new key is running,
the exposed key can also seal a secrets bundle into the enclave. At every restart, before the log's
rotation has been read, the delivery listener accepts only the key that the configuration names.
With rotation on, the listener accepts that key the whole time. A bundle that the exposed key seals
sets the webhook URLs that pages go to from the next restart. Nothing bounds its version, so a
bundle sealed at the highest version refuses every later delivery on this release. The refusal lasts
until a new release's own sealed file replaces it, or the host deletes the file.

1. With rotation off, rotate the key at once, as [Rotate the operator key](#rotate-the-operator-key)
   describes.

2. Download the archive, as
   [Download the public log and verify it](reference.md#download-the-public-log-and-verify-it)
   describes, and list every signed administrative instruction in it:

   ```shell
   jq -c 'select(.entry=="command" and .cmd.type=="signed_admin") | {seq, ts, command: .cmd.command, events: [.events[].type]}' archive/0*.jsonl
   ```

   Look for any instruction that you did not send.

3. Consider making the markets reduce-only while the exposed key is still valid, as
   [Make markets reduce-only](reduce-only.md) describes.

4. Ship a new release whose configuration names the new key, as
   [Upgrade to a new release](deploy-and-rollback.md#upgrade-to-a-new-release) describes. **The
   trading server refuses, at recovery, a configuration that names a key the log has not rotated
   to, so the rotation must be logged before the new release can take the log.**

5. Deliver the secrets bundle again, signed with the new key, as [Deliver secrets](#deliver-secrets)
   describes. The new release accepts a moved bundle only under the new key's signature.

6. Until the new release runs, run `secrets-status` after every restart, as
   [See what an enclave holds](#see-what-an-enclave-holds) describes. The sealed version must be the
   newest one you delivered. **Any other version means that the host is lying to you, or that the
   exposed key delivered a bundle. Treat both as a compromise.**

**When to escalate.** Tell the owner at once, with any administrative instruction from step 2 that
you did not send.

### The operator key is lost

Use this section when no copy of the operator key remains. Keep two copies of the key, in separate
places, so that this does not happen.

While rotation is off, no other key can sign an administrative instruction or a rotation. The
governance fallback in [When no standby qualifies](standby.md#when-no-standby-qualifies) restores
the trading server, and its `installOperator` step seats a new admin key, which only the timelock
can name.

While rotation is on, only an election replaces the operator, as
[Standing as an operator candidate](operator-candidate.md) describes. A trading server waiting for a
benchmark snapshot that the lost key would sign stays held. Run the whole fallback in
[When no standby qualifies](standby.md#when-no-standby-qualifies), which turns rotation off and
names a release of its own.

## Relayer

Use this procedure when `checkpoint_post_failed` or `checkpoint_relayer_low` fires because the
checkpoint relayer cannot pay for gas, or when the checkpoint endpoint's key is lost.

`checkpoints.relayer` is an account on the checkpoint RPC endpoint, which signs and pays for the
transactions that post checkpoints. It has no authority over funds, because only the enclave's
signature authorises a checkpoint. If its balance runs out, or the endpoint's key is lost, posts
fail and `checkpoint_post_failed` pages. With `[checkpoints.relayer_gas]`, which the production
template sets, `checkpoint_relayer_low` warns first, once the balance falls below
`checkpoints.relayer_gas.min_balance`. Withdrawals wait while no checkpoint posts.

1. Fund the relayer's account, or stand up a new endpoint with a new, funded relayer.

   Funding the existing account needs no release. **The relayer and the endpoint are part of the
   configuration, so changing either is a new release**, as
   [Deploy and rollback](deploy-and-rollback.md) describes.

**How to tell it is resolved.** `checkpoint_post_failed` and `checkpoint_relayer_low` resolve, and
`cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC` rises.

## Secrets

The trading server reads webhook URLs, vendor API keys and RPC credentials as secrets. The
configuration names each secret, and the operator delivers its value into the enclave. The module
documentation of `crates/node/src/secrets/mod.rs` describes the design, and what a host can do at
each step.

Keep the values in a bundle file on the administrative workstation, never on the trading server's
host, with one entry for every name that the configuration uses:

```toml
version = 1
[secrets]
provider_a_key = "..."
alerts_critical_url = "https://..."
```

`$SECRETS` is the delivery listener's address as the host forwards it, `tee.sealing.secrets.listen`.
The delivery tool checks the enclave's quote against `release.toml`, and encrypts the bundle only
after that check passes.

A real enclave accepts a bundle only under the signature of the operator key in effect:

- before the log is recovered, the key that the attested configuration names;
- once the trading server runs, the key that the log records;
- while rotation is on and an election that governs the key is in force, either handing over or
  with its key recorded in the log, only the key that its own configuration names. An elected key
  has no business in another operator's enclave.

A rotated operator's old key therefore stays the one that the enclave accepts at a restart, until a
release's configuration names the new key. An upgrade's new release accepts a moved bundle only
under the signature of the operator that its own configuration names. A bundle signed before a
rotation is refused there, and the operator delivers again. A fallback trading server whose
configuration names the key that the registry installed also accepts only that key, even before the
log mirrors the installation.

### See what an enclave holds

Use this command to read the versions that an enclave holds. It changes nothing.

```shell
$ENGINE admin --config node.toml --key-file operator.key \
  secrets-status --to $SECRETS --release release.toml
```

The command prints the sealed and running bundle versions, and the secrets the enclave lacks. The
sealed version is the newest one you delivered. **A lower version means that the host restored an
older sealed file. Treat the host as suspect, and deliver a higher version, which repairs it.**

### Deliver secrets

Use this procedure to deliver a bundle, for example when `secret_missing` fires.

1. Deliver the bundle.

   **Changes the live system.** The owner must approve the delivery.

   ```shell
   $ENGINE admin --config node.toml --key-file operator.key \
     deliver-secrets --to $SECRETS --release release.toml --secrets-file secrets.toml
   ```

   The command prints `sealed: version <n>`. It refuses a bundle that lacks a chain or screening key
   unless you add `--partial`, and it names any integration that stays off. A retry is safe. The
   same version with the same secrets is accepted as already sealed. A lower version, or the same
   version with other secrets, is refused. No receipt means that nothing is proven sealed.

   After an operator rotation, the configuration still names the old key, so signing with the new
   key takes `--rotated`, as for administrative instructions:

   ```shell
   $ENGINE admin --config node.toml --key-file operator.key --rotated \
     deliver-secrets --to $SECRETS --release release.toml --secrets-file secrets.toml
   ```

### Rotate a secret

Use this procedure to replace a secret's value. Raise `version`, and include every secret, because
a bundle replaces the whole sealed bundle. Add the new key at its provider first, and leave the old
one valid, because what runs keeps the clients it started with.

1. Deliver the bundle, as [Deliver secrets](#deliver-secrets) describes. The command prints
   `restart required`, `secrets_restart_required` fires as a Warning, and `secrets-status` shows the
   new sealed version beside the old running one.
2. Restart the trading server, as [Restart and recovery](restart-and-recovery.md) describes. The
   enclave's keys stay.

   **Changes the live system.** The owner must approve the restart.

   The alert resolves, and the sealed and running versions match.
3. Revoke the old key at its issuer.

### A secret is exposed

Use this procedure when a secret's value may be known to someone else. Treat it as lost now.

1. Revoke the secret at its issuer first: the vendor's console, the RPC provider, or the webhook
   receiver.
2. Rotate it, as [Rotate a secret](#rotate-a-secret) describes.

Revoking a secret has effects until the restart:

- Revoking a vendor key stops that vendor's feed, and `venue_feed_down` fires. A market with one
  vendor left keeps a price only if its `min_sources` allows it. Make the markets that fall below
  their `min_sources` reduce-only, as [Make markets reduce-only](reduce-only.md) describes.
- Revoking a custody provider's key takes that provider out of the quorum until the restart. With
  too few providers left, deposit crediting pauses, and `custody_rpc_quorum_lost` fires until then.

### A sealed secrets file is lost or does not open

Use this procedure when `secrets_unreadable` and `secret_missing` fire. For a chain or screening
key, the trading server holds at its next start and serves only the delivery listener.

1. Deliver the bundle again, as [Deliver secrets](#deliver-secrets) describes. The trading server
   continues starting with no restart.

### Secrets across releases and elections

A new release asks the running release for the bundle first, without stopping it, as
[Upgrade to a new release](deploy-and-rollback.md#upgrade-to-a-new-release) describes. An election
moves nothing. An elected operator delivers its own bundle to its own enclave, as
[Standing as an operator candidate](operator-candidate.md) describes.

The log records the access source's signing key with its freshness bound. Replacing that key needs a
fresh log, but replacing the source's endpoint does not.
