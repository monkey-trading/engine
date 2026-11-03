# Running as a standby

A standby is a registered, approved candidate that follows the operator in effect and is ready to
take over if the operator stops. It holds **no keys**. It copies the public log, so that when the
operator's checkpoints stop, the standby already has the log and can resume it under new keys of its
own. The takeover is a chain of steps across the registry, the vault and the standby; this runbook
orders them. Read [reference.md](reference.md) and [operator-candidate.md](operator-candidate.md) first.

Rotation must be on for any of this to apply. While it is off, a stalled operator is handled by the
fallback at the end of this page.

## How a takeover goes

1. The operator in effect stops posting checkpoints.
2. After the vault has gone without a checkpoint for the **liveness timeout** (3 days at launch;
   `parameters().livenessTimeout`), anyone may call the registry's `promote()`. It makes the top
   eligible standby (most delegated stake, the earlier registration on a tie) the pending operator and
   moves the vault's current release to that standby's **at once**. The standby's admin key takes
   effect after the notice (7 days plus the finality margin).
3. The standby's process sees the promotion, finalizes its copy of the log at the last checkpoint the
   vault accepted, drops the unsigned tail after it, and exits.
4. The standby's operator starts the registered release on the SEV-SNP host with `new-key`, and
   delivers that candidate's own secrets to it ([key-recovery.md](key-recovery.md), "Secrets"): no
   secret comes from the failed operator. It then serves only its attestation.
5. The candidate registers that enclave's address with `setEnclave`, and the timelock installs it with
   `setSigner` (3-day delay). The node then resumes at the last checkpoint the old key signed.

Between steps 1 and 5 no checkpoint is accepted and no withdrawal is authorised. The vault's
`exitPeriod` (14 days at launch) counts from the last checkpoint throughout: 3 days of liveness and
3 of timelock delay leave about a week of margin, and none of it for a standby that is not ready.

## Run the standby

The standby is an ordinary process on an ordinary host. It needs no enclave and the image has no
mode for it, because it holds no keys: it verifies every stream frame against the operator's
enclave address and copies public bytes. Run it on a host that shares no failure with the node and
with the operator.

1. **Config.** The standby's config is the candidate's release config plus a `[standby]` section. It
   is a different file from the registered release (different hash), which is fine: nothing is
   registered for it. It needs EVM custody and `[checkpoints]`, because it reads the vault and the
   registry. Its `operator.public_key` is the candidate's own admin key. No enclave runs it, so no
   secret is delivered to it: drop `[tee.sealing.secrets]`, write its RPC and screening URLs out in
   full, read its webhook URLs with `url_env` and any vendor key with `api_key_env`, and keep this file
   private, which a production build allows for `run --standby` on a config with `[standby]`.

   ```toml
   [standby]
   # The operator in effect's API origin: its /v1/stream and, last, its archive.
   operator = "https://api.operator.example"
   # Archive mirrors of the operator's log, tried in order before the operator, which may be dead
   # exactly when the archive matters. Every origin must serve the same public bytes.
   mirrors = ["https://mirror-a.example", "https://mirror-b.example"]
   # The enclave address the operator's VERIFIED attestation names. OWNER: set after verifying it.
   enclave = "0x..."
   poll_interval = "30s"
   ```

   A production build refuses `standby.danger_accept_invalid_certs`.
2. **Take the enclave address from a verified attestation**, never from the operator's say-so:
   `curl -sf https://api.operator.example/v1/attestation > op.json`, then `verify-attestation` against
   the operator's published `release.toml` ([reference.md](reference.md)), then
   `jq -r .enclave_address op.json`. Keep `op.json` and the release manifest: a replay across a change
   of signer needs one attestation per signer.
3. **Start it.** Webhook URLs come from the environment on this ordinary host. Keep the console
   output.

   ```sh
   $ENGINE run --standby --config standby.toml
   ```

   Starting it changes nothing on chain and nothing on the node, except that it adds one stream
   subscriber to the operator's API: tell the operator.
   Check: the console says `standby following the operator in effect`; the log directory fills; no
   `standby_follow_failed` page. A cold start backfills the completed segments from the mirrors first,
   then follows the stream.
4. **Check the copy keeps up**, daily and after every operator release change. Copy the directory
   first, because the standby is still writing it:

   ```sh
   cp -R data/log /tmp/copy && $ENGINE replay --log /tmp/copy
   cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC
   ```

   Check: `replay` ends `verified:` and its newest checkpoint's seq is at least the vault's
   `latestSeq()` minus the checkpoints in flight (one finality delay's worth). A copy that does not
   reach the vault's latest accepted checkpoint cannot take over.
5. **Follow the operator's changes.** If the operator's enclave key changes (a recovery in
   [key-recovery.md](key-recovery.md)) or an election hands the node to a new operator, the stream's
   signer changes and the standby stops accepting frames (`standby_follow_failed`). Verify the new
   attestation, change `enclave` and `operator`, and restart the standby.

| Alert | Meaning |
| --- | --- |
| `standby_follow_failed` (Critical) | The stream failed or a frame did not verify. The standby retries every `poll_interval`. A dead operator refuses connections at once, which is the case this exists for: it keeps the copy it has. |
| `operator_log_rollback` (Critical) | The operator's log no longer holds entries the standby copied: its signed head is behind the copy, its repeat of the copy's last command differs, or a command it sent does not chain onto the copy. The message names the range. The standby appends nothing more and keeps its copy, which holds the entries as the standby received them; it resolves by itself if the operator's log holds them again. The entries the stream carried are enclave-sealed, but a copy bootstrapped from an archive mirror is only hash-checked against that mirror: before restoring the operator from this copy, verify it with `engine replay --attestation` and treat a refusal as a poisoned copy, not as the operator's fault. Tell the operator and the owner, and keep a copy of the directory (`cp -R data/log /tmp/rollback-evidence`) before anything else touches it. |
| `standby_signal_failed` (Critical) | Reading the registry or the vault for a promotion failed. Check `[checkpoints].rpc_url`. |
| `standby_takeover_failed` (Critical) | A promotion arrived and the copy could not be finalized. See "After a promotion", step 3. |

## After a promotion

You are the standby of a promoted candidate. Move quickly: the clock in "How a takeover goes" runs.

1. **Confirm it is you.** `promote()` elects the top eligible standby, which may be another party's.

   ```sh
   cast send $REGISTRY 'promote()' --rpc-url $RPC --account <any>         # only if nobody has yet
   cast call $REGISTRY 'pendingElection()((uint64,address,address,bytes32,uint64,bool))' --rpc-url $RPC
   cast call $REGISTRY 'pendingPromotion()(bool)' --rpc-url $RPC
   ```

   **Changes the live system.** `promote()` needs the owner's approval of this contract call. It reverts
   with `LivenessNotDue(<time>)` before the timeout, `NoQualifiedStandby` when no eligible candidate
   other than the operator in effect exists, `ElectionPending` when one already waits, and
   `RotationDisabled`. Check: `pendingPromotion()` prints `true`, and the pending election's admin key
   is your admin key. If it names someone else, that standby takes over; yours stays a follower.
2. **Fold the release change into the vault**, then wait for `finalized`:

   ```sh
   cast send $VAULT 'syncRelease()' --rpc-url $RPC --account <any>
   cast call $VAULT 'currentRelease()(bytes32)' --rpc-url $RPC            # is your release's id
   ```

   **Changes the live system.** Needs the owner's approval. Without this fold, your `new-key` start
   waits a minute for it and refuses ("have the promoter call syncRelease").
3. **Let the standby finalize its copy.** Within `poll_interval` of the promotion being visible, it
   refreshes completed segments from the mirrors, requires the checkpoint the vault accepted last,
   cuts the copy after it, logs `promoted: the copy ends at the latest published checkpoint …`, and
   exits with status 0. If `standby_takeover_failed` fires instead, the copy is missing the vault's
   last accepted checkpoint: that checkpoint was posted after the stream last reached you and is not
   in the mirrors' completed segments yet. Fetch the operator's archive and the mirrors again (they
   may lag), then restart the standby; the cut is resumable and idempotent. A crash mid-cut leaves a
   `takeover.json` that the next start repairs.
4. **Check the copy**: `replay` on it ends in a checkpoint equal to the vault's accepted one
   (`latestSeq()` and `latestRoot()`).
5. **Start the registered release with new keys.** Copy the finalized log directory to the SEV-SNP
   host's data disk as `/data/log`, with `config.toml` (the registered release's config, **not** the
   standby's, byte for byte) and an empty file `new-key`. Boot the VM per
   [deploy-and-rollback.md](deploy-and-rollback.md), section 2, step 2.
   **Changes the live system.** Needs the owner's approval to deploy.
   Check: the node serves `/v1/attestation` and nothing else (`signer_unregistered`); on its first
   start it cuts the log to the vault's finalized checkpoint by itself.
6. **Verify its attestation** and take the new enclave address `$NEW` and its pin.
7. **Register the enclave**, from the candidate account. While rotation is on the vault's `setSigner`
   accepts only the address the operator in effect registered:

   ```sh
   cast send $REGISTRY 'setEnclave(address)' $NEW --rpc-url $RPC --account <candidate>
   cast call $REGISTRY 'operatorEnclave()(address)' --rpc-url $RPC       # equals $NEW
   ```

   **Changes the live system.** Needs the owner's approval of this contract call.
8. **Install it through the timelock** (pattern in reference.md):

   ```sh
   TARGET=$VAULT
   DATA=$(cast calldata 'setSigner(address)' $NEW)
   ```

   **Changes the live system.** Needs the owner's approval of this contract call. Any address but the
   registered one reverts with `NotTheRegisteredEnclave(<given>, <registered>)`. Schedule it the
   moment `$NEW` is known: it waits the timelock's delay.
   Check: `checkpointSigner()` equals `$NEW`; the node resumes at the last checkpoint the old key
   signed; `signer_unregistered` resolves; `latestSeq()` rises.
9. **Authorizations and replay.** Unexpired authorizations from the old key are reissued under the new
   epoch by themselves; each pays once. Replay across the change needs both attestations:

   ```sh
   $ENGINE replay --log archive --attestation op.json --release op-release.toml --attestation new.json --release release.toml --processor <name>
   ```

10. **The admin key.** From the promotion's effective time (7 days plus the margin), your admin key is
    the operator's: `curl -sf $API/v1/operator | jq .operator`. Until then admin commands stay with the old
    key, which may be dead. Sign with your own key and the config's chain and vault, without `--rotated`.

## When no standby qualifies

`promote()` reverts with `NoQualifiedStandby` when no eligible candidate other than the operator in
effect exists. The operator in effect continues. If it has failed and cannot be brought back
([restart-and-recovery.md](restart-and-recovery.md)), the governance fallback is the owner's
decision, and it is slow. The timelock does each step, with its delay, and nothing here can be
shortened:

1. Check the clock: the `exitPeriod` counts from the last checkpoint. Each timelock step is
   3 days. If the exit would start first, the owner decides whether to let it.
2. **Turn rotation off.** `setRotation(false)` cancels an election not yet in effect and ends the
   limit on `setSigner` and `setCurrentRelease`. Pattern in reference.md with `TARGET=$REGISTRY` and
   `DATA=$(cast calldata 'setRotation(bool)' false)`. **Changes the live system.** Needs the owner's
   approval of this contract call. Check: `rotationEnabled()` prints `false`; the node mirrors it.
3. **Name the fallback's release.** `setCurrentRelease(<fallback digest>, <fallback config hash>)`
   (deploy-and-rollback.md, section 2, step 1). **Changes the live system.** Needs the owner's approval.
4. **Boot the fallback** on its own host with the published log (a standby's copy or a mirror's)
   and `new-key`, verify its attestation, and have the timelock call `setSigner($NEW)`
   (key-recovery.md, "New keys", steps 3 to 5). It then resumes at the last checkpoint the old key
   signed.

The fallback must be decided before it is needed: a host, a built and approved release, an archive
mirror it can read, and the people who can sign the timelock calls.

**The admin key is not part of this.** While rotation is off, the key the log records changes only
by a `rotate-operator` signed by that key. If the failed operator's key is gone, the fallback runs
the node and the books but nothing can send an admin command (reduce-only, corporate actions) until
an election with rotation on, which needs the 90-day minimum term to have passed. The owner decides
this before launch ([operator-candidate.md](operator-candidate.md), "Open questions").
