# Deploy and rollback

Use this runbook to prepare a release, to deploy the contracts and the first release, to upgrade to
a new release, and to return to the previous one. A release is a launch measurement, which
identifies the image, together with a configuration file. The vault's timelock names one release as
the only release that may hold the enclave's keys. A first deployment, a new image and a changed
configuration therefore follow the same procedure. Even a one-line change to the configuration makes
a new release, because the configuration's SHA-256 is part of the release's identity.

The steps use the shell variables that [Variables](reference.md#variables) sets up. Commands and
their output call a running trading server a node. Every step marked **Changes the live system**
needs its own approval, and an approval for one step does not cover the next.

- [Know before you start](#know-before-you-start)
- [Prepare a release](#prepare-a-release)
- [Deploy the contracts](#deploy-the-contracts)
- [Deploy the rotation contracts](#deploy-the-rotation-contracts)
- [First deployment](#first-deployment)
- [Upgrade to a new release](#upgrade-to-a-new-release)
- [If the handover does not finish](#if-the-handover-does-not-finish)
- [Roll back](#roll-back)
- [Companion processes](#companion-processes)
- [Staging](#staging)

## Know before you start

- **The running release keeps sequencing until its successor has taken the keys.** Trading does
  not stop while governance names the new release. Do not stop the running release to speed things
  up. Once the vault names another release, the running release's own sealed keys no longer open,
  and stopping it loses them, as [Key recovery](key-recovery.md) describes.
- **A new release may not be able to go back.** The trading server activates the newest protocol
  version when it starts. A build older than a protocol version cannot replay a log that has
  activated that version. Compare `ProtocolVersion::LATEST` between the running tag and the new tag
  by running the following command once for each tag:

  ```shell
  git grep -h "const LATEST" <tag> -- crates/core/src/protocol.rs crates/core/src/protocol/
  ```

  If the two values differ, the upgrade cannot be rolled back to the old image, and a problem must
  be fixed with a further release.
- **While operator rotation is on, `setCurrentRelease` reverts with `RotationActive`.** The release
  that the vault names is then the release of the operator in effect. To name a release, the
  timelock approves it for the operator's candidacy, and the candidacy moves to it, as
  [Upgrade while you are the operator](operator-candidate.md#upgrade-while-you-are-the-operator)
  describes. The handover that follows is the one in
  [Upgrade to a new release](#upgrade-to-a-new-release).
- **While the boundary's log check applies, the keys carry the head that the check verified.** The
  keys also carry each release that handed them on past that head. A release whose build does not
  carry the log check, or does not record those releases, refuses keys that carry them. Its
  handover stops the running release, and then fails. The check applies only while rotation is on.
  It also needs the set of the turn in effect to name two or more members. Once the check no longer
  applies, the trading server drops the head and those releases from its sealed keys, and it
  refuses a handover until it has done so. With rotation off, an upgrade or a rollback to such a
  build therefore hands over as usual. While rotation is on, upgrade and roll back only between
  releases that carry the log check and record the releases that handed the keys on, as
  [Daily turns](operator-candidate.md#daily-turns) describes.
- **No staging environment exists.** Run each procedure against a testnet vault first. The steps
  are the same.

## Prepare a release

Use this procedure before any deployment or upgrade. None of its steps touches a live system.

1. On the `main` commit you intend to release, whose CI run passed, run the soak test against the
   live venues:

   ```shell
   scripts/dev-venue-load.sh --soak
   ```

   The soak passes when it ends with `test result: ok`, with no market left without a price for
   longer than 10 seconds while connections are dropped. **If the soak fails, stop. The commit is
   not fit to release.**

   Then run the node soak, eight hours at ten times launch flow on the launch book with each feed
   fault in turn and a restart from the newest cache every two hours, in a quiet window, where
   `<dir>` is a directory on a disk with room for its log:

   ```shell
   SOAK_DIR=<dir> nice scripts/dev-soak.sh 2>&1 | tee soak-report.txt
   ```

   The soak passes when every interval's report ends `verdict PASS` and the run ends with
   `test result: ok`. **If it fails, stop. The commit is not fit to release.** Keep the report for the
   release notes, and delete `<dir>` once it is saved.

2. Tag the commit `v<version>`, where `<version>` is the release's version number.

3. Take `vmlinuz`, `initramfs.cpio.gz`, `OVMF.fd`, `measurement.json` and `SHA256SUMS` from the
   `image-first` artifact of the commit's CI run. If the artifact has expired, produce it again:

   ```shell
   gh workflow run ci.yaml --ref v<version>
   ```

4. On a second machine, rebuild the image from the tag, where `<dir>` is a new output directory:

   ```shell
   scripts/build-image.sh --ref v<version> --no-cache <dir>
   ```

   Then compare the two builds, where `<ci>` is the directory holding the CI artifact:

   ```shell
   diff <dir>/SHA256SUMS <ci>/SHA256SUMS
   ```

   The builds match when `diff` prints nothing. **If they differ, stop. The image is not
   reproducible from the tag.**

5. Assemble the configuration from the template, the benchmark publishers and the production
   genesis:

   ```shell
   cat ops/runbooks/production-node.toml config/benchmark-publishers.toml \
     config/production-genesis.toml > node.toml
   ```

6. Fill in every value in `node.toml` that carries an `OWNER` or `REPLACE` marker in
   [production-node.toml](production-node.toml), and delete each marker comment as you set its
   value. Then confirm that no marker is left:

   ```shell
   grep -nE 'OWNER|REPLACE' node.toml
   ```

   The command prints nothing when every value is set.

   `[snapshots] trusted_signers` names the enclave address, which does not exist until a first
   release starts. A first release therefore leaves the whole `[snapshots]` section out: unpinned,
   every start would replay the whole log and also rebuild the history index from it. In each later
   release, set it to `$SIGNER`, the enclave address the running release attests and the upgrade
   carries over, so that the new release restarts from its caches. A key that changes, by a
   promotion or new keys, needs a release that pins the new address.

7. Confirm that the configuration loads. `engine admin` reads the configuration before it asks for
   a key, so the following command fails with the configuration's error, or reaches the key check
   when the configuration is valid:

   ```shell
   env -u OPERATOR_KEY -u OPERATOR_KEY_FILE $ENGINE admin --config node.toml \
     --pin $(printf '0%.0s' $(seq 64)) set-reduce-only false
   ```

   The configuration is valid when the last line is
   `Error: no operator key: pass --key-file, or set OPERATOR_KEY_FILE or OPERATOR_KEY`. If the first
   line is `Error: parsing <file>` or `Error: validating <file>`, the lines after `Caused by:` name
   the key to fix. `$ENGINE` must be built with the production feature, so that its configuration
   checks match the trading server's.

8. Hash the configuration:

   ```shell
   sha256sum node.toml
   ```

   The hash is `$CONFIG_HASH`, the second argument of `setCurrentRelease`. **Do not edit the file
   again. A single changed byte makes another release.**

9. Write `release.toml`, in the format that [Release](../../image/README.md#release) in the image's
   README shows. It holds the release name; `measurement.json`'s `launch_digest`, `policy` and
   `guest_features`; the minimum reported TCB; and `[config_hashes]` holding
   `"node.toml" = "0x<hash>"`. The minimum TCB is marked `OWNER`. Take it from the host's
   firmware. Confirm that `guest_features` is `0x201`, because a verifier refuses a release without
   Secure TSC.

10. Publish the GitHub release with the image files, `node.toml`, `release.toml` and the browser
    verifier package. The configuration holds no credential, only the names of the secrets that the
    operator delivers.

11. Before the new release goes live, ship a console build that pins the new `release.toml` beside
    the previous release's. The attestation binds the configuration, so a console that does not pin
    the new manifest refuses the new trading server.

12. If a previous release is running, copy its log off it now, as
    [Download the public log and verify it](reference.md#download-the-public-log-and-verify-it)
    describes. The successor starts from that log.

## Deploy the contracts

Use this procedure once, before the first deployment. **Changes the live system.** The owner must
approve each deployment. The constructor values are in
[production-deployment.toml](production-deployment.toml).

1. Deploy an OpenZeppelin `TimelockController`, with `[timelock] min_delay` as its delay and the
   deployment's multisig as its proposer. The delay must not be zero. Grant the executor and
   administrator roles deliberately, and once the timelock is set up, leave it administering
   itself. **The vault checks only that the timelock has code and a delay above zero. That check
   cannot show that the contract is the intended OpenZeppelin implementation, or that its roles
   belong to the intended multisig, so verify both yourself.** Later changes to the delay or the
   roles go through the timelock's own governance. Keep the intended delay and proposer policy.

2. Deploy `Vault`, with these constructor arguments in this order: the USDC token; the signer, which
   is a placeholder until [First deployment](#first-deployment) registers the enclave's address; the
   timelock; the multisig as guardian; and the `[vault]` values `period_budget`, `exit_period`,
   `claim_window` and `stale_after`. The vault deploys the MONKEY token and its exit verifier
   itself. The constructor reverts with `InvalidGovernance` when the timelock has no code or no
   delay. It reverts with `InvalidExitSchedule` when the exit period is no longer than the delay,
   when the claim window or the staleness limit is zero, or when the staleness limit is longer than
   the exit period. A new vault starts empty. No balance, accepted root or used nonce carries over
   from another vault, and the trading server reads the new vault's signer epoch and reconciles
   against it before it releases any withdrawal authorisation.

3. Deploy `OperatorRegistry`, with these constructor arguments in this order:

   - the MONKEY token that the vault deployed in step 2, whose address
     `cast call $VAULT 'monkey()(address)' --rpc-url $RPC` prints;
   - the `[staking]` values `cooldown` and `epoch`, in seconds, as `cooldownSeconds_` and
     `epochSeconds_`, with which the registry deploys the `Staking` contract;
   - the timelock;
   - the initial operator, which is the operator's admin key that `operator.public_key` names, with
     the launch digest and the configuration hash of the first release;
   - the parameters in force, which are the launch values that
     [How the operator is chosen](operator-candidate.md#how-the-operator-is-chosen) lists;
   - the lowest and the highest value that the timelock may later set for each parameter, which
     the owner chooses.

   The constructor reverts with `InvalidGovernance` when the timelock has no code or no delay, and
   with `ZeroAddress` when the initial admin key is zero. It reverts with `InvalidBounds` when the
   bounds are not valid, and with `ParameterOutOfBounds` when a parameter lies outside them.

4. Through the timelock, set the vault's registry with `setRegistry(address)` and the registry's
   vault with `setVault(address)`, as
   [Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock)
   describes. Each can be set only once.

## Deploy the rotation contracts

Use this procedure once, after [Deploy the contracts](#deploy-the-contracts) and before
[First deployment](#first-deployment). Several operators take daily turns from launch, and the turns
need six more contracts: the log heads, the seed receiver and relay, the operator schedule, the
parameter vote and the fault proof. **Changes the live system.** The owner must approve each
deployment and each timelock call. The constructor values are in
[production-deployment.toml](production-deployment.toml). Rotation stays off throughout, and the
timelock turns it on later, as
[Register the incumbent before rotation is turned on](operator-candidate.md#register-the-incumbent-before-rotation-is-turned-on)
describes.

Run the commands from `contracts/` in a checkout of the release's commit, with `<deployer>` the
Foundry keystore account that deploys. `$VAULT`, `$REGISTRY`, `$TIMELOCK` and `$RPC` are set as
[Variables](reference.md#variables) describes, and `$L1_RPC` is an Ethereum provider.

**The registry records the first release's configuration hash, and that configuration names these
contracts.** Compute each contract's address before [Deploy the contracts](#deploy-the-contracts)
deploys the registry, and write it into the configuration before you hash it. A contract's address
follows from the account that deploys it and that account's nonce at the deployment:

```shell
cast compute-address <deployer> --nonce <nonce>
```

Deploy each contract from that account at that nonce, and send nothing else from it in between.

1. **Deploy the log heads** on Arbitrum, naming the vault.

   **Changes the live system.** The owner must approve the deployment.

   ```shell
   forge create src/LogHeads.sol:LogHeads --rpc-url $RPC --account <deployer> --broadcast --constructor-args $VAULT
   ```

   Set `LOG_HEADS` to the address that the command prints after `Deployed to:`.

2. **Deploy the seed receiver and the seed relay,** as steps 1 to 3 of
   [Deploy the relay and receiver](seed-relay.md#deploy-the-relay-and-receiver) describe.

   **Changes the live system.** The owner must approve each deployment.

   Set `SEED_RECEIVER` to the receiver's address on Arbitrum and `SEED_RELAY` to the relay's address on
   Ethereum.

3. **Deploy the operator schedule** on Arbitrum, with the registry, the seed receiver, the log heads
   and the `[schedule]` values.

   **Changes the live system.** The owner must approve the deployment.

   ```shell
   forge create src/OperatorSchedule.sol:OperatorSchedule --rpc-url $RPC --account <deployer> --broadcast --constructor-args $REGISTRY $SEED_RECEIVER $LOG_HEADS "(140,40,3,200)" "(90,30,3,120)" "(140,60,5,256)" "(2,2,4)" 20 250
   ```

   After the three addresses come the heartbeat rule in force, its minimums and its maximums, each as
   the interval, the head tolerance, the missed slots and the held-head blocks. Then come the take
   window as its slots, minimum and maximum, the switch budget in seconds, and Arbitrum's block time in
   milliseconds. The constructor reverts with `InvalidLiveness` or `InvalidTakeWindow` when the values
   break the schedule's limits. Set `SCHEDULE` to the address that the command prints, and check it:

   ```shell
   cast call $SCHEDULE 'registry()(address)' --rpc-url $RPC
   cast call $SCHEDULE 'liveness()(uint64,uint64,uint16,uint16)' --rpc-url $RPC
   cast call $SCHEDULE 'takeWindow()(uint16,uint16,uint16)' --rpc-url $RPC
   ```

   The first command prints `$REGISTRY`, the second `140`, `40`, `3` and `200`, and the third `2`, `2`
   and `4`.

4. **Name the schedule in the registry,** through the timelock, as
   [Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock)
   describes, with these values:

   ```shell
   TARGET=$REGISTRY
   DATA=$(cast calldata 'setSchedule(address)' $SCHEDULE)
   ```

   **Changes the live system.** The owner must approve this contract call, and the multisig signs the
   schedule and the execute.

   The call reverts with `ScheduleAlreadySet` once a schedule is named, with `RotationActive` while
   rotation is on, with `EpochTooShort` while the election epoch is shorter than a day, and with
   `InvalidSchedule` for a schedule built for another registry. Once it executes,
   `cast call $REGISTRY 'schedule()(address)' --rpc-url $RPC` prints `$SCHEDULE`.

5. **Deploy the parameter vote** on Arbitrum, over the schedule, with the `[parameter_vote]` windows.

   **Changes the live system.** The owner must approve the deployment.

   ```shell
   forge create src/ParameterVote.sol:ParameterVote --rpc-url $RPC --account <deployer> --broadcast --constructor-args $SCHEDULE 259200 3600
   ```

   Set `VOTE` to the address that the command prints.
   `cast call $VOTE 'schedule()(address)' --rpc-url $RPC` prints `$SCHEDULE`.

6. **Deploy the fault proof** on Arbitrum, naming the vault.

   **Changes the live system.** The owner must approve the deployment.

   ```shell
   forge create src/FaultProof.sol:FaultProof --rpc-url $RPC --account <deployer> --broadcast --constructor-args $VAULT
   ```

   Set `FAULT_PROOF` to the address that the command prints.

7. **Name the contracts in the release's configuration,** before
   [Prepare a release](#prepare-a-release) hashes it. The configuration is part of the release, so a
   contract named later needs a new release. In `node.toml`, built from
   [production-node.toml](production-node.toml), set these keys. The template carries the last three
   commented out: uncomment them, since without them the release mirrors no turn.

   - `log_heads.contract` to `$LOG_HEADS`;
   - `seed_relay.receiver` to `$SEED_RECEIVER`, and `seed_relay.relay` to `$SEED_RELAY`;
   - `custody.evm.staking.registry.schedule` to `$SCHEDULE`;
   - `custody.evm.staking.registry.vote.contract` to `$VOTE`, with its `restriction_ttl`;
   - `custody.evm.staking.registry.fault_proof.contract` to `$FAULT_PROOF`, with its `penalty`.

**How to tell it is resolved.** The registry names the schedule, every check above prints what it
should, and checks 2 to 5 in [Variables and checks](seed-relay.md#variables-and-checks) pass. The
relayer's checks there need a running trading server, so they wait for the first deployment.

## First deployment

Use this procedure to bring up the first release on a new vault.

Before step 1, complete and check each of the following:

- The contracts are deployed and wired as [Deploy the contracts](#deploy-the-contracts) and
  [Deploy the rotation contracts](#deploy-the-rotation-contracts) describe, with the vault's signer
  still a placeholder. Check the values against the chain, where `$STAKING`
  is `custody.evm.staking.contract`:

  ```shell
  cast call $VAULT 'exitPeriod()(uint64)' --rpc-url $RPC
  cast call $VAULT 'claimWindow()(uint64)' --rpc-url $RPC
  cast call $VAULT 'staleAfter()(uint64)' --rpc-url $RPC
  cast call $TIMELOCK 'getMinDelay()(uint256)' --rpc-url $RPC
  cast call $STAKING 'epoch()(uint256)' --rpc-url $RPC
  cast call $STAKING 'cooldown()(uint256)' --rpc-url $RPC
  ```

  Each value equals the one in [production-deployment.toml](production-deployment.toml).
- The host is a bare-metal AMD SEV-SNP machine.
- DNS for the API domain and a CAA record are in place, as
  [Before the first certificate](certificate-renewal-failure.md#before-the-first-certificate)
  describes.
- A receiver for Critical alerts is running.
- The [companion processes](#companion-processes) are planned.

1. **Name the release.** The timelock calls `setCurrentRelease(launch_digest, config_hash)` on the
   vault, following
   [Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock)
   with these values:

   ```shell
   DIGEST=0x...                                   # measurement.json: launch_digest (48 bytes)
   CONFIG_HASH=0x$(sha256sum node.toml | cut -d' ' -f1)
   TARGET=$VAULT
   DATA=$(cast calldata 'setCurrentRelease(bytes,bytes32)' $DIGEST $CONFIG_HASH)
   ```

   **Changes the live system.** The owner must approve this contract call, and then the multisig
   signs the schedule and the execute.

   Confirm the release that the vault names:

   ```shell
   cast call $VAULT 'currentRelease()(bytes32)' --rpc-url $RPC
   cast keccak $(cast concat-hex $DIGEST $CONFIG_HASH)
   cast call $VAULT 'releaseGeneration()(uint64)' --rpc-url $RPC
   ```

   The first two commands print the same hash, and the generation has risen by one. Wait until the
   block holding the execute is `finalized`, about 15 minutes after its batch is posted, as
   [Chain finality](reference.md#chain-finality) describes. The trading server reads the release at
   that block.

2. **Boot with new keys.** Make the data disk with `config.toml`, which is the file you hashed,
   renamed, and an empty file named `new-key`. Start the VM on the host with the launch flags in
   `measurement.json`, as [Boot on an SEV-SNP host](../../image/README.md#boot-on-an-sev-snp-host)
   describes, and forward `tee.sealing.secrets.listen` from the operator's addresses.

   **Changes the live system.** The owner must approve the deployment of this image and
   configuration.

   The trading server then holds, raising `secret_missing` for each secret, with only its delivery
   listener up. It continues once it has the chain access that the secrets give it.

3. **Deliver the secrets,** as [Secrets](key-recovery.md#secrets) describes.

   **Changes the live system.** The owner must approve the delivery.

   The delivery tool prints `sealed: version 1`. Then `curl -sk $API/v1/attestation` answers, while
   `/v1/markets` does not. The serial console shows `signer_unregistered`, which is expected because
   the vault does not yet name this key. If the console shows `secrets_start_failed` instead, a
   provider refused a delivered key: see [Alerts](alerts.md).

4. **Verify the attestation** against `release.toml`, and take `$SIGNER` and `$PIN` from it, as
   [The trading server's attested key and its pin](reference.md#the-trading-servers-attested-key-and-its-pin)
   describes. `verify-attestation` exits 0 and prints
   `verified: release <name>, enclave 0x…, config node.toml`. The configuration it names is the
   `[config_hashes]` entry whose hash is `$CONFIG_HASH`.

5. **Register the signer.** The timelock calls `setSigner($SIGNER)`, with these values:

   ```shell
   TARGET=$VAULT
   DATA=$(cast calldata 'setSigner(address)' $SIGNER)
   ```

   **Changes the live system.** The owner must approve this contract call, and the multisig signs
   it. `setSigner` raises `signerEpoch`.

   Confirm the signer:

   ```shell
   cast call $VAULT 'checkpointSigner()(address)' --rpc-url $RPC
   ```

   The command prints `$SIGNER`. Once the block is `finalized`, the trading server starts by itself.
   It reads the vault every `checkpoints.retry_interval`, so `signer_unregistered` resolves within
   seconds of finality, and `/v1/markets` answers once the trading server has recovered its log.

6. **Let the first start finish.** On its first trusted time sample from the Roughtime servers, the
   trading server activates the newest protocol version and sequences
   `config/production-genesis.toml` as administrative instructions. Confirm both:

   ```shell
   curl -sf $API/v1/markets | jq length
   curl -sf $API/v1/house/state | jq .seq
   ```

   The first command prints the number of launch markets, and the second prints a number that rises
   between runs. **A `genesis_refused` or `protocol_activation_failed` page means that the
   configuration's genesis does not apply to the log. Do not edit the log. Fix the configuration and
   make a new release.**

7. **Check the certificate.** `curl -sf $API/v1/health` succeeds without `-k`, and the following
   command prints `0`:

   ```shell
   curl -s http://<metrics host>:9464/metrics | grep engine_tls_certificate_pending
   ```

   `<metrics host>` is the address of the trading server's metrics listener. If either check fails,
   follow [Certificate renewal failure](certificate-renewal-failure.md).

8. **Check checkpoints and withdrawals.** After a few minutes of trading or a test deposit,
   `cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC` rises, and the metric
   `engine_withdrawals_paused` reads `0`. The metric reads `1` until the first reconciliation, which
   is normal. If it stays at `1`, follow [Withdrawal stalls](withdrawal-stalls.md).

9. **Record the emission schedule.** The values are the owner's, held in
   [production-deployment.toml](production-deployment.toml). `$START` is that file's `start`,
   00:00:00 UTC on the day that restricted mainnet trading opens. It is the origin of both emission
   period 0 and staking epoch 0. The same file's `[vault]` and `[staking]` values are the ones the
   contracts were deployed with.

   **Changes the live system.** The owner must approve the exact command, including its values, and
   a `$START` that is midnight UTC on the opening day. **Sign this instruction before `$START`, and
   only once for each log. The schedule can never be reset.**

   ```shell
   START=2027-01-01T00:00:00Z   # production-deployment.toml: start
   $ENGINE admin --config node.toml --node $API --pin $PIN init-emissions --start $START --period-budget 100000000000000000000000
   ```

   The budget is 100,000 MONKEY a period, which is 10^23 base units, and it caps the maximum supply
   at 5,256,000,000 MONKEY. It must equal the vault's budget, and the trading server refuses any
   other value with `emission_budget_mismatch`.

10. **Record the staking configuration,** with the same `$START`.

    **Changes the live system.** The owner must approve the exact command, including its values.
    **Sign this instruction before `$START`, and only once for each log.**

    ```shell
    $ENGINE admin --config node.toml --node $API --pin $PIN init-staking --start $START --liquidity-floor 30500000 --epoch-ms 86400000 --cooldown-ms 2419200000 --secrets-file secrets.toml
    ```

    The epoch is one day and the cooldown is 28 days. They are the staking contract's own `epoch()`
    and `cooldown()`, in milliseconds. The liquidity floor is $30.5 million. The tool reads the
    contract through the configuration's custody providers, and refuses a different value before it
    signs. The trading server reads the contract again, and refuses a different value before it
    sequences the instruction. `--secrets-file` is the bundle delivered at step 3, whose keys fill
    in the providers' URLs for that read. A refusal names both values. **Fix the command, never the
    contract.**

    Confirm both instructions:

    ```shell
    curl -sf $API/v1/monkey | jq '{e: .emissions, s: .staking, d: .distribution}'
    ```

    The output shows `start_ms` equal to `$START` in milliseconds, `period_budget`
    `100000000000000000000000`, `cooldown_ms` `2419200000`, `epoch_ms` `86400000` and
    `liquidity_floor` `30500000`. The engine rejects a second run of either instruction.

11. **Deposit house capital, if any.** House capital enters only through a vault deposit to the
    house, `depositToHouse`.

    **Changes the live system.** The owner must approve the amount.

    The launch amount is zero. The production genesis sets a house exposure cap floor for every
    launch market and a total floor of $61 million, so trading opens with no house capital.

## Upgrade to a new release

Use this procedure to move to a new image, a new configuration, or both. The running release is
`A`, and the new release is `B`.

A release whose requests for the keys name the protocol version that its build reads cannot take
the keys from a running release whose requests do not, because the running release cannot parse the
request. That upgrade, with rotation on or off, goes through the timelock's new-key path, from the
step that boots with new keys in [New keys](key-recovery.md#new-keys), instead of the `predecessor`
file below.

1. **Prepare `B`,** as [Prepare a release](#prepare-a-release) describes. Settle the protocol
   version check in [Know before you start](#know-before-you-start), so that you know whether the
   upgrade can be undone.

2. **Name `B`.** Follow step 1 of [First deployment](#first-deployment) with `B`'s digest and
   configuration hash. With rotation on, follow steps 2 to 4 of
   [Upgrade while you are the operator](operator-candidate.md#upgrade-while-you-are-the-operator)
   instead.

   **Changes the live system.** The owner must approve this contract call.

   With rotation off, `setCurrentRelease` moves only the vault's release. Also move the registry's
   operator release, either by the candidacy's move to an approved `B` or by `installOperator`
   again. **Otherwise, the next `setRotation(true)` makes the vault name `A` again.** Before every
   `setRotation(true)`, confirm that the third field of `operatorInEffect()`, the operator's
   release, equals the output of `cast call $VAULT 'currentRelease()(bytes32)' --rpc-url $RPC`.

   Within a minute of the block becoming `finalized`, `A` raises `release_superseded`, a Critical
   alert, and keeps sequencing. The page is expected. It means that a restart of `A` would now find
   no keys.

3. **Prepare `B`'s disk.** On the new host or disk, place `B`'s configuration as `config.toml`, a
   file named `predecessor` that holds `A`'s handover address as `host:port`, which is
   `tee.sealing.handover.listen`, and the log you copied at step 12 of
   [Prepare a release](#prepare-a-release), in `/data/log`. Confirm that `A`'s handover port is
   reachable from `B`'s network, and that `B`'s `[tee.sealing.handover]` and `[tee.sealing.secrets]`
   are configured in the same way as `A`'s.

4. **Start `B`.**

   **Changes the live system.** The owner must approve the deployment of this image and
   configuration, and the handing of the enclave's keys to it.

   `B` first asks `A` for the delivered secrets, without stopping `A`. `A` checks `B`'s quote
   against the vendor's certificate chain and the release that the vault names, and sends the bundle
   encrypted to `B`. `B` accepts it only under the signature of the operator that `B`'s
   configuration names. If the move fails, `B` raises `secret_missing`, and you deliver the secrets
   to `B` yourself, as [Secrets](key-recovery.md#secrets) describes. `A` refuses to move the secrets
   while rotation is on, and a bundle signed before an operator rotation does not move.

   `B` then connects to `A` for the keys. `A` checks `B`'s quote in the same way, stops sequencing,
   signs a final checkpoint, and sends the keys and the log's tail encrypted to `B`. `B` writes the
   log, checks that it reaches that checkpoint, seals the keys and confirms. `A` acknowledges and
   powers off. With rotation on, `A` is a member of the rotation set, so it follows `B` instead of
   powering off, and it opens none of its keys again. Once `B` serves, power `A`'s VM off if `A` is
   not standing for a later turn.

   Confirm the upgrade:

   - `B` serves `/v1/markets`.
   - `secrets-status` on `B` shows the version that `A` held.
   - `curl -sf $API/v1/attestation | jq -r .enclave_address` still prints `$SIGNER`.
   - `A`'s VM has powered off, and `release_superseded` has resolved.
   - The chain of posted checkpoints continues:
     `cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC` rises.

   The TLS key carries over, so `$PIN` is unchanged. The certificate does not carry over. `B` orders
   its own certificate for the same key, under the same ACME account, and serves it once the CA
   issues it.

5. **Watch for a page that says the handover did not finish.** If `handover_unconfirmed`,
   `handover_failed` or a refused start of `B` appears, follow
   [If the handover does not finish](#if-the-handover-does-not-finish). **Do not stop anything.**

6. **Update the companion processes.** Point the Certificate Transparency watcher at `B`'s
   `release.toml` at once, or it raises `attestation_unverified`. Pin the benchmark worker again
   only if the pin changed. See [Companion processes](#companion-processes).

7. **Keep `A`'s disk until `B` has run for one full day,** and then wipe it. `A`'s sealed copy can
   never open again, and it holds nothing that `B` needs.

## If the handover does not finish

Use this section when `handover_unconfirmed` or `handover_failed` fires during an upgrade, or when
`B` refuses to start. The handover moves the enclave's keys from `A` to `B`. While it is incomplete,
sequencing may have stopped, and clients cannot trade. **Do not stop either release. Stopping `A`
before a successor holds the keys loses the keys.**

| What you see | What it means | What to do |
| --- | --- | --- |
| `handover_unconfirmed` on `A` | Sequencing has stopped, and no successor holds the keys yet. `A` keeps serving successors. | Start `B` again. |
| `handover_unconfirmed` on `B` | `B` has sealed the keys, but `A` did not acknowledge, so `A` may still be running. | Wait until `A` has powered off, and then start `B` again. |
| `handover_failed` on `A` | `A` stopped without a successor, and its keys no longer open. | Follow [The running release stopped](key-recovery.md#the-running-release-stopped). |
| `B` refuses to start, naming a log that does not reach the checkpoint | The log on `B`'s disk ends before `A`'s final checkpoint. | Copy the log again, as step 12 of [Prepare a release](#prepare-a-release) describes, and start `B` again. |

**How to tell it is resolved.** `B` serves `/v1/markets`, `release_superseded` has resolved, and
`latestSeq()` rises, as step 4 of [Upgrade to a new release](#upgrade-to-a-new-release) describes.

**When to escalate.** If `handover_failed` fires, tell the owner at once. New keys need the
timelock, and its delay runs against the vault's exit period.

## Roll back

Use this section when a new release must be undone. Choose the case that matches where the upgrade
stands.

### `A` is still running and `B` has not taken the keys

To call the upgrade off:

1. The timelock calls `setCurrentRelease` again, with `A`'s digest and configuration hash, as step 1
   of [First deployment](#first-deployment) describes. With rotation on, follow the rollback in
   [Upgrade while you are the operator](operator-candidate.md#upgrade-while-you-are-the-operator)
   instead.

   **Changes the live system.** The owner must approve this contract call.

   The call starts another generation. `A`'s sealed keys belong to an earlier generation, so `A`
   keeps raising `release_superseded`.

2. To make `A` safe to restart again, start a fresh copy of `A`'s image and configuration with a
   `predecessor` file that points at the running `A`, as step 4 of
   [Upgrade to a new release](#upgrade-to-a-new-release) describes.

   **Changes the live system.** The owner must approve this deployment.

   The fresh copy serves, and `release_superseded` resolves. The old VM powers off. With rotation
   on, the old VM follows the fresh copy instead, so power it off yourself.

### `B` has the keys and is running

Rolling back is then another upgrade, to `A`'s release.

1. Check the protocol version rule in [Know before you start](#know-before-you-start). If `B`
   activated a newer protocol version, no rollback is possible. Ship a fix as a further release,
   `C`.
2. Follow [Upgrade to a new release](#upgrade-to-a-new-release) with `A`'s image and configuration
   as the new release. The vault names it in a new generation, so `A` takes the keys afresh from
   `B`. Give `A` a disk with the log alone, as that procedure's step 3 does, never `B`'s
   `/data/snapshots`. A release refuses to start on a recovery cache in a format it does not read,
   with `signed snapshot payload is malformed`, and an earlier release may not read the caches `B`
   wrote, such as one that carries unpaid withdrawals. If `A` stops with that error, move
   `/data/snapshots` aside and start it again: it replays the whole log and rebuilds its history
   index.

### A configuration value in `B` is the problem

A wrong feed, limit or provider in `B`'s configuration also needs a new release, because a running
trading server's configuration cannot be changed. A secret's value is not a configuration value.
Rotating a secret needs a delivery and a restart, but no new release, as
[Secrets](key-recovery.md#secrets) describes.

## Companion processes

These processes run outside the enclave, and none of them holds the enclave's keys.

### The benchmark worker

With `[benchmarks]` in the configuration, the trading server charges no rollover after a start until a
signed snapshot of the publishers' rates has applied. A start that finds a daily rollover unpaid waits
for the snapshot before it sequences ordinary instructions, so the missed rollovers are charged at its
rates. A start with every rollover paid, such as a turn's handover, sequences at once, and waits only
if its next rollover comes before the snapshot does. The benchmark worker fetches and signs those
snapshots.

1. On a host of its own, with `OPERATOR_KEY_FILE` set for this process alone, start the worker:

   **Changes the live system.** The worker signs benchmark rates with the operator's key and
   sequences them into the live engine, so the owner must approve starting it and the key that it
   signs with.

   ```shell
   $ENGINE admin --config node.toml --node $API --pin $PIN sync-benchmarks --watch
   ```

   Start it before the trading server, or as soon as the trading server serves its attestation. The
   worker retries until the trading server answers.

2. Restart the worker after every restart of the trading server that changes the pin.

With `--watch`, the worker fetches and signs every `benchmarks.interval`, which is hourly in the
template, so that a failed fetch is retried and a late publication arrives before the rollover. It
also signs before each rollover, and at once when the trading server's rollover schedule changes. A
failed or refused submission is retried within 10 seconds. Without `--watch`, the worker signs one
snapshot and exits. An administrative instruction signed while the worker fetches takes the
worker's nonce, so that attempt is refused and the next one goes through.

After a long outage, a snapshot carries every missed day, up to 16,384 observations. **If the
trading server refuses the snapshot as too large, with status 413, a trading server waiting for a
snapshot stays held until `api.max_body_bytes` is raised, which needs a new release.**

The worker raises `benchmark_sync_failed`, `benchmark_fetch_failed`, `benchmark_sync_pending` and
`benchmark_rate_refused`. It reads a webhook's URL only from an environment variable that the
configuration names with `url_env`, never from a delivered secret. The production template's
`[alerts]` routes name delivered secrets with `url_secret`, and a production build refuses `url_env`
in the trading server's configuration. **Under a production configuration, the worker therefore
posts no alert, and its alerts appear only in its own log, so watch that log.**

The worker keeps its own alert outbox beside the trading server's, in a directory named after the
outbox directory with `-benchmark-worker` added. For the production template's `/data/alerts`, the
directory is `/data/alerts-benchmark-worker`. The worker opens the outbox before it signs anything,
so the path must be creatable, writable by the worker's user, and persistent on the worker's own
host. **If it is not, the worker exits with an error naming the path and signs nothing, and a
restarted trading server sequences no ordinary instruction until a snapshot arrives.**

### The Certificate Transparency watcher

Run the watcher on a host independent of the trading server's, and start it before the first
certificate is issued. Its configuration template is `config/ct-watch.toml`.

```shell
$ENGINE ct-watch --config ct-watch.toml
```

[`unattested_certificate`](unattested-certificate.md) covers its alerts.

### Monitoring and archive mirrors

- **Prometheus** scrapes the trading server's metrics listener and the watcher's, with the rules in
  `ops/prometheus/engine-node-rules.yml`. Route its alerts to a receiver other than the trading
  server's own webhook. The Grafana dashboard is `ops/grafana/engine-node.json`.
- **Two archive mirrors** that do not share a failure each run the download loop in
  [Download the public log and verify it](reference.md#download-the-public-log-and-verify-it). A
  standby and a successor operator read the log from them when the trading server is down.

## Staging

Run [First deployment](#first-deployment), [Upgrade to a new release](#upgrade-to-a-new-release)
and [Roll back](#roll-back) against a testnet vault and a second host. Use the same image, and a
configuration that differs only in its chain, vault and providers. The procedures are proven when
each completes without a step that this runbook does not describe.
