# Running as a standby

Use this runbook when you run a standby for a registered and approved candidate, when a liveness
promotion names your candidate, or when the operator has failed and no standby qualifies. A standby
follows the operator in effect and is ready to take over if the operator stops. It holds no keys. It
copies the public log, so that when the operator's checkpoints stop, the standby already holds the
log and can resume it under new keys of its own. The takeover is a chain of steps across the
registry, the vault and the standby, and this runbook puts them in order. Read
[Reference](reference.md) and [Standing as an operator candidate](operator-candidate.md) first.
Commands and their output call a running trading server a node.

- [How a takeover goes](#how-a-takeover-goes)
- [Run the standby](#run-the-standby)
- [After a promotion](#after-a-promotion)
- [When no standby qualifies](#when-no-standby-qualifies)

The standby takeover applies only while rotation is on, and only to a registry that elects its
operator. While rotation is off, a stalled operator is handled by the fallback in
[When no standby qualifies](#when-no-standby-qualifies). A registry that names a schedule rotates
the operator in daily turns instead. There, the registry's `promote()` reverts with
`ScheduleNamesTurns`, and the schedule's own `promote()`, which the members' liveness callers send,
replaces a failed operator within minutes, as
[Minute-scale liveness](operator-candidate.md#minute-scale-liveness) describes.

## How a takeover goes

1. The operator in effect stops posting checkpoints.
2. Once the vault has gone without a checkpoint for the liveness timeout, anyone may call the
   registry's `promote()`. The timeout is 3 days at launch, and the registry's
   `parameters().livenessTimeout` holds it. The promotion makes the top eligible standby the pending
   operator. The top standby is the one with the most delegated stake, or the earlier registration
   on a tie. The promotion moves the vault's current release to that standby's release at once. The
   standby's admin key takes effect after the seven-day notice plus the finality margin.
3. The standby process sees the promotion, finalises its copy of the log at the last checkpoint that
   the vault accepted, drops the unsigned tail after that checkpoint, and exits.
4. The standby's operator starts the registered release on the SEV-SNP host with `new-key`, and
   delivers the candidate's own secrets to it, as [Secrets](key-recovery.md#secrets) describes. No
   secret comes from the failed operator. The new trading server then serves only its attestation.
5. The candidate registers that enclave's address with `setEnclave`, and the timelock installs it
   with `setSigner`, after the timelock's 3-day delay. The trading server then resumes at the last
   checkpoint that the old key signed.

From step 1 to step 5, the vault accepts no checkpoint and no withdrawal is authorised. The vault's
`exitPeriod`, 14 days at launch, counts from the last checkpoint throughout. The 3 days of the
liveness timeout and the 3 days of the timelock's delay leave about a week of margin. **A standby
that is not ready when the promotion comes has none of that margin.**

## Run the standby

Use this procedure to set up a standby for a candidate and keep it following the operator.

The standby is an ordinary process on an ordinary host. It needs no enclave, and the image has no
mode for it, because it holds no keys. It verifies every stream frame against the operator's enclave
address, and copies public bytes. Run it on a host that shares no failure with the operator's
trading server or with the operator.

1. Write the standby's configuration. It is the candidate's release configuration plus a `[standby]`
   section. It is a different file from the registered release, with a different hash, and that is
   expected, because nothing is registered for it.

   - It needs EVM custody and `[checkpoints]`, because it reads the vault and the registry.
   - Its `operator.public_key` is the candidate's own admin key.
   - No enclave runs it, so no secret is delivered to it. Remove `[tee.sealing.secrets]`, write its
     RPC and screening URLs out in full, read its webhook URLs with `url_env`, and keep the file
     private. A production build allows this for `run --standby` on a configuration with
     `[standby]`.
   - A standby runs no feeds, so the vendor connections stay as written, including the CME
     connection and the names of its OAuth secrets. A configuration with `[standby]` loads without a
     listener for the secrets they name, and nothing reads them. The same file started without
     `--standby` is refused, because a trading server that runs feeds must be able to receive their
     keys.
   - With `operator.rotation_enabled = true`, the configuration also needs `[log_heads]`, as every
     rotation-set member's does.

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

   With `operator.rotation_enabled = true`, the standby does not keep to `operator` and `enclave`.
   Before each stint, it reads the operator in effect from the registry, reaches it at `https://`
   and its registered API hostname, and verifies it against the vault's signer at the latest block.
   It therefore moves with each daily turn and each election, and with a `setSigner` replacement at
   an unchanged hostname. That includes archive recovery and the copy's first signer. A promotion
   replays the copy against every signer that the vault recorded, rather than against `enclave`. A
   stint that fails within five minutes of a move logs a warning rather than paging, while the new
   operator comes up. The mirrors and the certificate setting in `[standby]` still apply.

2. Take the operator's enclave address from a verified attestation, never from the operator's word.

   ```shell
   curl -sf <operator API>/v1/attestation > op.json
   $ENGINE verify-attestation --attestation op.json --release op-release.toml \
     --processor <Genoa|Milan>
   jq -r .enclave_address op.json
   ```

   `<operator API>` is the operator's API origin, as in `standby.operator`. `op-release.toml` is the
   release manifest that the operator published, and `--processor` is the codename of the operator
   host's CPU generation, as
   [The trading server's attested key and its pin](reference.md#the-trading-servers-attested-key-and-its-pin) describes.
   `verify-attestation` exits with status 0, and the last command prints the enclave address to put
   in `enclave`. **Keep `op.json` and the release manifest.** A replay across a change of signer
   needs one attestation per signer.

3. Start the standby. On this ordinary host, the webhook URLs come from the environment.

   ```shell
   $ENGINE run --standby --config standby.toml
   ```

   Starting the standby changes nothing on chain and nothing on the operator's trading server,
   except that it adds one stream subscriber to the operator's API. Tell the operator. Keep the
   console output.

   The console prints `standby following the operator in effect`, the log directory fills, and no
   `standby_follow_failed` page arrives. A cold start first backfills the completed segments from
   the mirrors, and then follows the stream.

4. Check that the copy keeps up, daily and after every change of the operator's release. Copy the
   directory first, because the standby is still writing it.

   ```shell
   cp -R data/log /tmp/copy && $ENGINE replay --log /tmp/copy
   cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC
   ```

   `replay` ends with `verified:`. Its newest checkpoint's seq is at least the vault's
   `latestSeq()`, less the checkpoints in flight, which is one finality delay's worth. **A copy that
   does not reach the vault's latest accepted checkpoint cannot take over.**

5. Follow the operator's changes. If the operator's enclave key changes, as in a recovery that
   [Key recovery](key-recovery.md) describes, or an election hands the trading server to a new
   operator, the stream's signer changes. The standby then stops accepting frames and raises
   `standby_follow_failed`. Verify the new attestation as in step 2, change `enclave` and
   `operator`, and restart the standby.

**How to tell it is resolved.** The standby follows the stream, and step 4's replay reaches the
vault's latest accepted checkpoint.

| Alert | Meaning | What to do |
| --- | --- | --- |
| `standby_follow_failed`, Critical | The stream failed, or a frame did not verify. The standby retries every `poll_interval`. A dead operator refuses connections at once, which is the case the standby exists for, and the standby keeps the copy it has. | If the operator's signer changed, follow step 5. |
| `operator_log_rollback`, Critical | The operator's log no longer holds entries that the standby copied. Its signed head is behind the copy, its repeat of the copy's last command differs, or a command it sent does not chain onto the copy. The message names the range. The standby appends nothing more and keeps its copy, which holds the entries as the standby received them. The alert resolves by itself if the operator's log holds them again. | Keep a copy of the directory before anything else touches it, with `cp -R data/log /tmp/rollback-evidence`. Tell the operator and the owner. The entries that the stream carried are sealed by the enclave, but a copy bootstrapped from an archive mirror is only checked by hash against that mirror. Before you restore the operator from this copy, verify it with `engine replay --attestation`, and treat a refusal as a poisoned copy, not as the operator's fault. |
| `standby_signal_failed`, Critical | Reading the registry or the vault for a promotion failed. | Check `[checkpoints].rpc_url`. |
| `standby_takeover_failed`, Critical | A promotion arrived, and the copy could not be finalised. | Follow step 4 of [After a promotion](#after-a-promotion). |

## After a promotion

Use this procedure when a liveness promotion names your candidate. **Move quickly, because the clock
in [How a takeover goes](#how-a-takeover-goes) is running.**

1. If nobody has promoted yet, send the promotion. Anyone may send it.

   **Changes the live system.** The owner must approve this contract call.

   ```shell
   cast send $REGISTRY 'promote()' --rpc-url $RPC --account <any>
   ```

   `<any>` is any funded keystore. The transaction reverts with one of these errors:

   - `LivenessNotDue(<time>)`, before the liveness timeout;
   - `NoQualifiedStandby`, when no eligible candidate exists other than the operator in effect;
   - `ElectionPending`, when an election already waits;
   - `RotationDisabled`, while rotation is off.

2. Confirm that the promotion names you. `promote()` elects the top eligible standby, which may be
   another party's.

   ```shell
   cast call $REGISTRY 'pendingElection()((uint64,address,address,bytes32,uint64,bool))' \
     --rpc-url $RPC
   cast call $REGISTRY 'pendingPromotion()(bool)' --rpc-url $RPC
   ```

   `pendingPromotion()` prints `true`, and the pending election's admin key, its third field, is
   your admin key. If it names someone else, that standby takes over, and yours stays a follower.

3. Fold the release change into the vault, and then wait for its block to be `finalized`.

   **Changes the live system.** The owner must approve this contract call.

   ```shell
   cast send $VAULT 'syncRelease()' --rpc-url $RPC --account <any>
   cast call $VAULT 'currentRelease()(bytes32)' --rpc-url $RPC
   ```

   The second command prints your release's id. Without this fold, your `new-key` start waits a
   minute for the fold at finality, and then refuses with a message that ends
   `call syncRelease (anyone may) and start again once its block is finalized`.

4. Let the standby finalise its copy. Within `poll_interval` of the promotion becoming visible, the
   standby refreshes the completed segments from the mirrors, requires the checkpoint that the vault
   accepted last, and cuts the copy after that checkpoint. It then logs
   `promoted: the copy ends at the latest published checkpoint …`, and exits with status 0.

   If `standby_takeover_failed` fires instead, the copy is missing the vault's last accepted
   checkpoint. That checkpoint was posted after the stream last reached you, and is not yet in the
   mirrors' completed segments. Fetch the operator's archive and the mirrors again, because they may
   lag, and then restart the standby. The cut can be resumed and repeated safely. A crash in the
   middle of the cut leaves a `takeover.json` file, which the next start repairs.

5. Check the copy. Replay it, and read the vault's accepted checkpoint:

   ```shell
   $ENGINE replay --log data/log
   cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC
   cast call $VAULT 'latestRoot()(bytes32)' --rpc-url $RPC
   ```

   The replay ends at a checkpoint equal to the vault's accepted one, with the same seq and root.

6. Start the registered release with new keys. Copy the finalised log directory to the SEV-SNP
   host's data disk as `/data/log`. Beside it, put `config.toml` and an empty file named `new-key`.
   **`config.toml` is the registered release's configuration, byte for byte, not the standby's.**
   Boot the VM as step 2 of [First deployment](deploy-and-rollback.md#first-deployment) describes.

   **Changes the live system.** The owner must approve the deployment.

   The trading server serves `/v1/attestation` and nothing else, and raises `signer_unregistered`.
   On its first start, it cuts the log to the vault's finalised checkpoint by itself.

7. Verify the new trading server's attestation, and take its enclave address as `$NEW` and its pin,
   as [The trading server's attested key and its pin](reference.md#the-trading-servers-attested-key-and-its-pin)
   describes.

8. Register the enclave from the candidate account. While rotation is on, the vault's `setSigner`
   accepts only the address that the operator in effect registered.

   **Changes the live system.** The owner must approve this contract call.

   ```shell
   cast send $REGISTRY 'setEnclave(address)' $NEW --rpc-url $RPC --account <candidate>
   cast call $REGISTRY 'operatorEnclave()(address)' --rpc-url $RPC
   ```

   The second command prints `$NEW`.

9. Have the timelock install the enclave, with the pattern in
   [Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock)
   and this target and data. **Schedule it the moment `$NEW` is known, because it waits the
   timelock's delay.**

   **Changes the live system.** The owner must approve this contract call.

   ```shell
   TARGET=$VAULT
   DATA=$(cast calldata 'setSigner(address)' $NEW)
   ```

   Any address other than the registered one reverts with
   `NotTheRegisteredEnclave(<given>, <registered>)`. Once the call executes, `checkpointSigner()`
   prints `$NEW`. The trading server resumes at the last checkpoint that the old key signed,
   `signer_unregistered` resolves, and `latestSeq()` rises.

10. Replay the log across the change of signer. Unexpired authorisations from the old key are
    reissued under the new epoch by themselves, and each is paid once. A replay across the change
    needs both attestations:

    ```shell
    $ENGINE replay --log archive \
      --attestation op.json --release op-release.toml \
      --attestation new.json --release release.toml --processor <name>
    ```

    `new.json` is the new enclave's attestation, `release.toml` is your release's manifest, and
    `<name>` is the processor's codename. The replay ends with `verified:`.

11. Take over the admin key. From the promotion's effective time, the seven-day notice plus the
    finality margin, your admin key is the operator's. Until then, administrative instructions stay
    with the old key, which may be dead. Sign with your own key and the configuration's chain and
    vault, without `--rotated`. To confirm the key in effect, run:

    ```shell
    curl -sf $API/v1/operator | jq .operator
    ```

    The command prints your admin key's address once the effective time has passed.

    **With `[benchmarks]`, the trading server's start holds for a snapshot that the old key signs.
    If that key is dead, the hold never ends, because the staking mirror that would bring your key
    in waits behind it.** In that case, run the whole governance fallback instead, as steps 2 to 6
    of [When no standby qualifies](#when-no-standby-qualifies) describe, with your own candidate and
    key. Turning rotation off removes the release that the registry named, and a configuration with
    `operator.rotation_enabled = true` is refused once rotation is off. The timelock therefore names
    a release whose configuration has your admin key and `operator.rotation_enabled = false`, that
    release starts with `new-key`, and the timelock installs your key and calls `setSigner`.
    Schedule the calls together, because each waits the timelock's delay while the exit period runs.

**How to tell it is resolved.** `checkpointSigner()` prints `$NEW`, `latestSeq()` rises, and
`GET /v1/operator` names your admin key once the effective time has passed.

**When to escalate.** Tell the owner at once if `standby_takeover_failed` persists after the mirrors
have been fetched again, or if the old key is dead and the `[benchmarks]` hold applies.

## When no standby qualifies

Use this procedure when `promote()` reverts with `NoQualifiedStandby`, because no eligible candidate
exists other than the operator in effect, and the failed operator cannot be brought back with
[Restart and recovery](restart-and-recovery.md). The operator in effect continues in the role.

The governance fallback is the owner's decision, and it is slow. The timelock performs each step,
with its delay, and nothing here can be shortened. **Decide the fallback before it is needed.** It
needs a host, a built and approved release, an archive mirror that it can read, the fallback
operator's admin key, and the people who can sign the timelock calls.

1. Check the clock. The `exitPeriod` counts from the last checkpoint, and each timelock step takes 3
   days. If the exit would start first, the owner decides whether to let it start.

   ```shell
   cast call $VAULT 'lastCheckpointAt()(uint64)' --rpc-url $RPC
   cast call $VAULT 'exitPeriod()(uint64)' --rpc-url $RPC
   cast call $TIMELOCK 'getMinDelay()(uint256)' --rpc-url $RPC
   ```

2. Turn rotation off, with the pattern in
   [Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock)
   and this target and data. `setRotation(false)` cancels an election that is not yet in effect, and
   lifts the limits on `setSigner` and `setCurrentRelease`.

   **Changes the live system.** The owner must approve this contract call.

   ```shell
   TARGET=$REGISTRY
   DATA=$(cast calldata 'setRotation(bool)' false)
   ```

   Once the call executes, `cast call $REGISTRY 'rotationEnabled()(bool)' --rpc-url $RPC` prints
   `false`, and the trading server mirrors the change.

3. Name the fallback's release, as step 1 of
   [First deployment](deploy-and-rollback.md#first-deployment) describes.

   **Changes the live system.** The owner must approve this contract call.

   ```shell
   TARGET=$VAULT
   DATA=$(cast calldata 'setCurrentRelease(bytes,bytes32)' <fallback digest> <fallback config hash>)
   ```

   `<fallback digest>` is the fallback release's launch measurement, and `<fallback config hash>` is
   its configuration's SHA-256.

4. Install the fallback's operator. With rotation off, the timelock alone can seat the new
   operator's admin key, so administrative instructions work even if the failed operator's key is
   gone, as [The operator key is lost](key-recovery.md#the-operator-key-is-lost) describes.

   **Changes the live system.** The owner must approve this contract call. Schedule it no later than
   step 5's `setSigner`, and execute it first.

   ```shell
   TARGET=$REGISTRY
   DATA=$(cast calldata 'installOperator(address,address,bytes,bytes32)' \
     <candidate> <admin key> <fallback digest> <fallback config hash>)
   ```

   `<candidate>` is the fallback's candidate address, and `<admin key>` is its admin key's address.
   The registry reverts with `RotationActive` while rotation is on, and with `TurnsPending(<until>)`
   while drawn turns still run. On chain, the installation takes effect at once. The engine mirrors
   it, and the installed key signs administrative instructions seven days after the mirror is
   logged, with no further step.

   Once the call executes,
   `cast call $REGISTRY 'operatorInEffect()((address,address,bytes32,uint64))' --rpc-url $RPC` names
   the new key, and `curl -sf $API/v1/operator | jq .operator` shows it from the effective time.

   The installation leaves the failed operator's candidacy listed and approved. To upgrade once
   rotation is on again, the installed operator needs a candidacy of its own, as
   [Upgrade while you are the operator](operator-candidate.md#upgrade-while-you-are-the-operator)
   describes. If the failed operator holds the one place past a full candidate cap, the installed
   operator is listed only after the timelock withdraws the failed operator's approval or raises the
   cap.

5. Boot the fallback on its own host, with the published log from a standby's copy or a mirror, and
   with `new-key`. Verify its attestation, and have the timelock call `setSigner($NEW)`, as steps 3
   to 5 of [New keys](key-recovery.md#new-keys) describe.

   **Changes the live system.** The owner must approve the deployment and the contract call.

   Once the installation is final, the trading server resumes at the last checkpoint that the old
   key signed. A trading server that the vault names before then refuses to start with
   `log_recovery_failed` and the message `the log records operator …`. Start it again once the
   installation is final.

6. The fallback's configuration names the new admin key. Its hash is the configuration hash that
   steps 3 and 4 name, so the key is fixed with the release.

   - Recovery admits a configuration that names the key the registry installed, read at the vault's
     finalised block, before the log has mirrored the installation.
   - Until that key is due, seven days after its mirror is logged, no benchmark snapshot hold waits
     on the failed key, and `benchmark_sync_skipped` fires. The trading server sequences, the mirror
     lands, and financing uses the last applied rates.
   - A trading server that is already holding for a snapshot when the installation becomes final
     stays held. Restart it.

     **Changes the live system.** The owner must approve the restart.

   - Start the benchmark worker under the new key once the effective time of the pending rotation
     has passed. From then, a hold waits for that key's snapshot. To read the effective time, run
     `curl -sf $API/v1/operator | jq .pending_rotation`.

7. Before rotation is turned on again, have the timelock withdraw every release approved for the
   failed operator, which delists it. Use the pattern in
   [Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock)
   for each release.

   **Changes the live system.** The owner must approve each contract call.

   ```shell
   TARGET=$REGISTRY
   DATA=$(cast calldata 'setReleaseApproval(address,bytes,bytes32,bool)' \
     <failed candidate> <digest> <config hash> false)
   ```

   **The installation leaves the failed operator's candidacy listed and approved, and the
   delegations stay with it.** The installed operator serves no minimum term, so the next
   `closeEpoch` would elect the failed operator if it holds the most votes, and its lost key would
   become the admin key seven days later. A liveness promotion would also pick it as the top
   eligible standby.

8. Confirm, for each release that the failed candidate held, that the approval is withdrawn and the
   candidate is no longer listed:

   ```shell
   cast call $REGISTRY 'releaseApproved(bytes32)(bool)' \
     $(cast keccak $(cast abi-encode 'f(address,bytes32)' <failed candidate> <release id>)) \
     --rpc-url $RPC
   cast call $REGISTRY 'candidateCount()(uint256)' --rpc-url $RPC
   cast call $REGISTRY 'candidateList(uint256)(address)' <i> --rpc-url $RPC
   ```

   The first command prints `false`. `<i>` runs from 0 to one less than the count that the second
   command prints, and no `candidateList` entry names the failed candidate.

**How to tell it is resolved.** The fallback's trading server sequences under the installed key, and
the release check in
[Register the incumbent before rotation is turned on](operator-candidate.md#register-the-incumbent-before-rotation-is-turned-on)
holds before rotation is turned on again.
