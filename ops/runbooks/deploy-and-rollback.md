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
  handover stops the running release, and then fails. The check applies only while a turn is in
  effect whose set names two or more members, which turning rotation off leaves in place until an
  installation ends the turns. Once the check no longer applies, the trading server drops the head
  and those releases from its sealed keys, and it refuses a handover until it has done so. Once the
  turns have ended, an upgrade or a rollback to such a build therefore hands over as usual. Until
  then, upgrade and roll back only between releases that carry the log check and record the releases
  that handed the keys on, as [Daily turns](operator-candidate.md#daily-turns) describes.
- **Staging is not yet running.** [Staging](#staging) runs each release for 24 hours before
  production does, once its host is in place. Until then, run each procedure against a testnet vault
  first. The steps are the same.

## Prepare a release

Use this procedure before any deployment or upgrade. The release workflow builds a release's files
from a tag of `main` and publishes them as a draft GitHub release, as
[Release](../../image/README.md#release) in the image's README describes. None of the steps below
touches a live system, but a tag and a published release need the owner's approval.

1. **Set the configurations.** Through a pull request, set every value in
   `config/release/production.toml` and `config/release/staging.toml` that carries an `OWNER` or
   `REPLACE` marker, and delete each marker comment as you set its value. The release ships every
   file in `config/release/`, an operator candidate's included. For a first deployment, the
   contracts' addresses are the ones that [Deploy the contracts](#deploy-the-contracts) plans. CI
   loads each configuration as a production build does. It also holds staging to production's
   values, apart from its chain, its contracts, accounts and providers, and its domain. It holds
   production to [production-node.toml](production-node.toml) in the same way, apart from the host's
   product names, the rotation contracts and `[snapshots]`. A value decided for every operator, such
   as `confirmations`, therefore changes in `production-node.toml` too, where it keeps its marker
   until every operator has set it. Confirm that no marker is left on `main`:

   ```shell
   grep -nE '# (OWNER|REPLACE)' config/release/*.toml
   ```

   The command prints nothing when every value is set. The release workflow refuses a tag while any
   marker is left.

   The release configurations leave `[snapshots]` out, because its `trusted_signers` names the
   enclave address, which does not exist until a first release starts. Unpinned, every start would
   replay the whole log and also rebuild the history index from it. In each later release while rotation is off
   (`operator.rotation_enabled = false`), add the section from [production-node.toml](production-node.toml) to each configuration, with
   `trusted_signers` set to `$SIGNER`, the enclave address that configuration's own running release
   attests and the upgrade carries over, so that the new release restarts from its caches. Staging's
   address differs from production's. A key that changes, by a promotion or new keys, needs a
   release that pins the new address.

   A configuration with `operator.rotation_enabled = true` leaves `[snapshots]` out: a rotation-set member
   with it recovers from disk at its turn instead of starting from its replica
   ([operator-candidate.md](operator-candidate.md)).

2. **Soak the commit.** Choose the `main` commit you intend to release. Its CI run must have passed,
   and that run must be a push to `main`, the nightly run or a manual run, because only those run
   every job. On that commit, run the soak test against the live venues:

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

3. **Run the release as a dry run** on the commit, where `<sha>` is the commit:

   ```shell
   gh workflow run release.yaml -f commit=<sha>
   ```

   The dry run builds the image and the browser verifier twice each, compares the builds, and writes
   the configurations and `release.toml`. It publishes nothing. The run passes when it succeeds and
   reports no warning. A warning names what would refuse the tag, such as a marker left in a
   configuration. The run's summary shows the manifest and the release notes, and its `release`
   artifact holds the files.

4. **Tag the commit.** The owner must approve the tag and its version, `v<MAJOR>.<MINOR>.<PATCH>`,
   starting at `v0.1.0`:

   ```shell
   git tag v<version> <sha>
   git push origin v<version>
   ```

   The release workflow refuses the tag unless it names a commit of `main` whose CI run passed, as
   step 2 describes, and the configurations carry no marker. It then builds and checks the files as
   the dry run did, gives each one a GitHub artifact attestation, and creates a draft release. If it
   refused the tag only because the commit's CI run had not finished, re-run the workflow once that
   run has passed. For any other refusal, fix the cause on `main` and tag the fixed commit with the
   next version.

5. **Publish the release.** Review the draft's files and notes, add the soak report to the notes,
   and publish it.

   The owner must approve publishing the release.

6. **Rebuild the image on the host,** in a clone of the repository, where `<dir>` is a new output
   directory and `<release>` holds the release's files:

   ```shell
   git fetch origin main --tags
   gh release download v<version> --repo monkey-trading/engine --dir <release>
   scripts/build-image.sh --ref v<version> --no-cache <dir>
   diff <dir>/SHA256SUMS <release>/SHA256SUMS
   ```

   The builds match when `diff` prints nothing. **If they differ, stop. The image is not
   reproducible from the tag, and the host must not boot it.** Then, in the same clone, check each
   file's attestation, and the files against their sums:

   ```shell
   for file in <release>/*; do
     gh attestation verify "$file" --repo monkey-trading/engine --deny-self-hosted-runners \
       --signer-workflow monkey-trading/engine/.github/workflows/release.yaml \
       --source-ref refs/tags/v<version>
   done
   (cd <release> && sha256sum -c SHA256SUMS engine-verify.SHA256SUMS)
   ```

   Every file verifies and every sum prints `OK`. A tag runs its own commit's release workflow, so
   also check that the tag's commit is on `main`:

   ```shell
   git merge-base --is-ancestor v<version> origin/main && echo "on main"
   ```

   The command prints `on main`. **If any check fails, stop.**

7. **Take the environment's configuration.** The procedures below call it `node.toml`: copy the
   release's `production.toml` to it, or `staging.toml` in staging. Its SHA-256 is the
   environment's entry in the release's `release.toml`, and is `$CONFIG_HASH`, the second argument
   of `setCurrentRelease`:

   ```shell
   cp <release>/production.toml node.toml
   sha256sum node.toml
   ```

   **Do not edit the file. A single changed byte makes another release.**

8. Before the new release goes live, ship a console build that pins the new `release.toml` beside
   the previous release's. The attestation binds the configuration, so a console that does not pin
   the new manifest refuses the new trading server.

9. If a previous release is running, copy its log off it now, as
   [Download the public log and verify it](reference.md#download-the-public-log-and-verify-it)
   describes. The successor starts from that log.

## Deploy the contracts

Use this procedure once, before the first deployment. **Changes the live system.** The owner must
approve each deployment. The constructor values are in
[production-deployment.toml](production-deployment.toml).

**Plan the addresses after step 1, before step 2.** Step 4 records the first release's configuration
hash in the registry, and that configuration names the vault, the minter, the registry, its staking
contract and the contracts that [Deploy the rotation contracts](#deploy-the-rotation-contracts)
deploys. Deploy those eight contracts on Arbitrum from one deployer account that sends nothing else in
between, in this order: the vault, the minter, the registry, the log heads, the seed receiver, the
operator schedule, the parameter vote and the fault proof. `<deployer>` is that account's Foundry
keystore name. Compute the eight addresses in that order from the account's address and its next
nonce:

```shell
DEPLOYER=$(cast wallet address --account <deployer>)
NONCE=$(cast nonce $DEPLOYER --rpc-url $RPC)
for i in 0 1 2 3 4 5 6 7; do cast compute-address $DEPLOYER --nonce $((NONCE + i)); done
```

The minter deploys the MONKEY token itself, at `cast compute-address <minter address> --nonce 1`, and
the registry deploys its staking contract itself, at `cast compute-address <registry address> --nonce 1`. The seed relay is on Ethereum, at the address
that [Deploy the relay and receiver](seed-relay.md#deploy-the-relay-and-receiver) computes. Write
each address into `config/release/production.toml`, as step 1 of
[Prepare a release](#prepare-a-release) describes, before the release is tagged. The release
configuration names the schedule, the vote and the fault proof, since without them the release
mirrors no turn.

- `custody.vault`, `custody.evm.minter` and `custody.evm.staking.registry.contract`;
- `custody.evm.staking.contract`, the staking contract;
- `log_heads.contract`, and `seed_relay.receiver` and `seed_relay.relay`;
- `custody.evm.staking.registry.schedule`;
- `custody.evm.staking.registry.vote.contract`, with its `restriction_ttl`;
- `custody.evm.staking.registry.fault_proof.contract`, with its `penalty`.

After each deployment, check that the address it printed is the planned one. **If one differs, stop:
the release's configuration names a contract that does not exist, and the release must be prepared
again before the registry records it.**

1. Deploy an OpenZeppelin `TimelockController`, with `[timelock] min_delay` as its delay and the
   deployment's multisig as its proposer. The delay must not be zero. Grant the executor and
   administrator roles deliberately, and once the timelock is set up, leave it administering
   itself. **The vault checks only that the timelock has code and a delay above zero. That check
   cannot show that the contract is the intended OpenZeppelin implementation, or that its roles
   belong to the intended multisig, so verify both yourself.** Later changes to the delay or the
   roles go through the timelock's own governance. Keep the intended delay and proposer policy.

2. Deploy `Vault`, with these constructor arguments in this order: the USDC token; the signer, which
   is a placeholder until [First deployment](#first-deployment) registers the enclave's address; the
   timelock; the multisig as guardian; and the `[vault]` values `exit_period`, `claim_window` and
   `stale_after`. The vault deploys its exit verifier itself. The constructor reverts with
   `InvalidGovernance` when the timelock has no code or no delay. It reverts with `InvalidExitSchedule` when the exit period is no longer than the delay,
   when the claim window or the staleness limit is zero, or when the staleness limit is longer than
   the exit period. A new vault starts empty. No balance, accepted root or used nonce carries over
   from another vault, and the trading server reads the new vault's signer epoch and reconciles
   against it before it releases any withdrawal authorisation.

3. Deploy `MonkeyMinter`, MONKEY's only minter for good, with the timelock and the `[minter]` value
   `period_budget`. It deploys the MONKEY token, capped at `2 x 26,280 x period_budget`. The
   constructor reverts with `InvalidGovernance` when the timelock has no code or no delay, and with
   `InvalidPeriodBudget` for a budget of zero or one whose cap would not fit 128 bits. It mints
   nothing until the timelock names the vault in step 5. From `contracts/` in a checkout of the
   release's commit:

   ```shell
   forge create src/MonkeyMinter.sol:MonkeyMinter --rpc-url $RPC --account <deployer> --broadcast --constructor-args $TIMELOCK 100000000000000000000000
   ```

   Set `MINTER` to the address that the command prints, which must be the planned one.

4. Deploy `OperatorRegistry`, with these constructor arguments in this order:

   - the MONKEY token that the minter deployed in step 3, whose address
     `cast call $MINTER 'monkey()(address)' --rpc-url $RPC` prints;
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

5. Through the timelock, set the vault's registry with `setRegistry(address)` and the minter's vault
   with `setVault(address)`, as
   [Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock)
   describes. The vault's registry can be set only once. The minter takes only a vault under the same
   timelock, and moves to another vault only once the guardian has retired the current vault's signer. The registry names the vault with the schedule,
   in step 4 of [Deploy the rotation contracts](#deploy-the-rotation-contracts).

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
keystore name of the account that deployed the vault, the minter and the registry. `$VAULT`,
`$REGISTRY`, `$TIMELOCK` and `$RPC` are set as [Variables](reference.md#variables) describes, and
`$L1_RPC` is an Ethereum provider. Each contract lands at the address that
[Deploy the contracts](#deploy-the-contracts) planned for it, which the release's configuration
already names.

1. **Deploy the log heads** on Arbitrum, naming the vault.

   **Changes the live system.** The owner must approve the deployment.

   ```shell
   forge create src/LogHeads.sol:LogHeads --rpc-url $RPC --account <deployer> --broadcast --constructor-args $VAULT
   ```

   Set `LOG_HEADS` to the address that the command prints after `Deployed to:`, which must be the
   planned one.

2. **Deploy the seed receiver and the seed relay,** as steps 1 to 3 of
   [Deploy the relay and receiver](seed-relay.md#deploy-the-relay-and-receiver) describe, with the
   receiver deployed from `<deployer>` at its planned address.

   **Changes the live system.** The owner must approve each deployment.

   Set `SEED_RECEIVER` to the receiver's address on Arbitrum and `SEED_RELAY` to the relay's address
   on Ethereum.

3. **Deploy the operator schedule** on Arbitrum, with the registry, the seed receiver, the log heads
   and the `[schedule]` values.

   **Changes the live system.** The owner must approve the deployment.

   ```shell
   forge create src/OperatorSchedule.sol:OperatorSchedule --rpc-url $RPC --account <deployer> --broadcast --constructor-args $REGISTRY $SEED_RECEIVER $LOG_HEADS "(140,40,3,200)" "(90,30,3,120)" "(140,60,5,256)" "(2,2,4)" 20 250
   ```

   After the three addresses come the heartbeat rule in force, its minimums and its maximums, each
   as the interval, the head tolerance, the missed slots and the held-head blocks. Then come the
   take window as its slots, minimum and maximum, the switch budget in seconds, and Arbitrum's block
   time in milliseconds. The constructor reverts with `InvalidLiveness` or `InvalidTakeWindow` when
   the values break the schedule's limits. Set `SCHEDULE` to the address that the command prints,
   which must be the planned one, and check it:

   ```shell
   cast call $SCHEDULE 'registry()(address)' --rpc-url $RPC
   cast call $SCHEDULE 'liveness()(uint64,uint64,uint16,uint16)' --rpc-url $RPC
   cast call $SCHEDULE 'takeWindow()(uint16,uint16,uint16)' --rpc-url $RPC
   ```

   The first command prints `$REGISTRY`, the second `140`, `40`, `3` and `200`, and the third `2`,
   `2` and `4`.

4. **Name the vault and the schedule in the registry,** through the timelock, as
   [Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock)
   describes, with these values:

   ```shell
   TARGET=$REGISTRY
   DATA=$(cast calldata 'setCustody(address,address)' $VAULT $SCHEDULE)
   ```

   **Changes the live system.** The owner must approve this contract call, and the multisig signs
   the schedule and the execute.

   The call reverts with `RotationActive` while rotation is on, with `TurnsPending` while a drawn
   turn of the schedule the registry follows or of the one the call names runs, with
   `EpochTooShort` while the election epoch is shorter than a day, with `ZeroAddress` for a vault
   with no code, without a reason for a contract that does not answer
   `checkpointSigner()`, with `VaultSignerLive` when it would change the vault or the schedule of a
   registry whose vault's signer the guardian has not retired, other than to name a first schedule,
   and with `InvalidSchedule` for a schedule built for another registry or
   over log heads that follow another vault, or for no schedule once one is named. Once it executes,
   `cast call $REGISTRY 'vault()(address)' --rpc-url $RPC` prints `$VAULT` and
   `cast call $REGISTRY 'schedule()(address)' --rpc-url $RPC` prints `$SCHEDULE`. The timelock may
   move the registry to another vault and schedule later, but only while rotation is off and no drawn
   turn of either schedule runs, and only once the guardian has retired the signer of the vault the
   registry follows.

5. **Deploy the parameter vote** on Arbitrum, over the schedule, with the `[parameter_vote]`
   windows.

   **Changes the live system.** The owner must approve the deployment.

   ```shell
   forge create src/ParameterVote.sol:ParameterVote --rpc-url $RPC --account <deployer> --broadcast --constructor-args $SCHEDULE 259200 3600
   ```

   Set `VOTE` to the address that the command prints, which must be the planned one.
   `cast call $VOTE 'schedule()(address)' --rpc-url $RPC` prints `$SCHEDULE`.

6. **Deploy the fault proof** on Arbitrum, naming the vault.

   **Changes the live system.** The owner must approve the deployment.

   ```shell
   forge create src/FaultProof.sol:FaultProof --rpc-url $RPC --account <deployer> --broadcast --constructor-args $VAULT
   ```

   Set `FAULT_PROOF` to the address that the command prints, which must be the planned one.

**How to tell it is resolved.** The registry names the schedule, every contract is at the address
that the release's configuration names, every check above prints what it should, and checks 2 to 5
in [Variables and checks](seed-relay.md#variables-and-checks) pass. The relayer's checks there need
a running trading server, so they wait for the first deployment.

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
  cast call $MINTER 'vault()(address)' --rpc-url $RPC
  cast call $STAKING 'epoch()(uint256)' --rpc-url $RPC
  cast call $STAKING 'cooldown()(uint256)' --rpc-url $RPC
  ```

  Each value equals the one in [production-deployment.toml](production-deployment.toml), and the
  minter's vault is `$VAULT`.
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

2. **Boot with new keys.** Make the log disk, a second disk on the host's other NVMe drive than the
   data disk's, which the image mounts at `/data/log`, as [the production template](production-node.toml)
   says; a production build refuses a configuration whose log directory shares a device with the
   history index, the recovery caches or the daily snapshots. Make the data disk with `config.toml`, which is the file you hashed,
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
   `verified: release <name>, enclave 0x…, config production.toml`, or `config staging.toml` in
   staging. The configuration it names is the `[config_hashes]` entry whose hash is `$CONFIG_HASH`.

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
   period 0 and staking epoch 0. The same file's `[minter]` and `[staking]` values are the ones the
   contracts were deployed with.

   **Changes the live system.** The owner must approve the exact command, including its values, and
   a `$START` that is midnight UTC on the opening day. **Sign this instruction before `$START`, and
   only once for each log. The schedule can never be reset.**

   ```shell
   START=2027-01-01T00:00:00Z   # production-deployment.toml: start
   $ENGINE admin --config node.toml --node $API --pin $PIN init-emissions --start $START --period-budget 100000000000000000000000
   ```

   The budget is 100,000 MONKEY a period, which is 10^23 base units, and it caps the maximum supply
   at 5,256,000,000 MONKEY. It must equal the minter's budget, and the trading server refuses any
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
   `tee.sealing.handover.listen`, and the log you copied at step 9 of
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
| `B` refuses to start, naming a log that does not reach the checkpoint | The log on `B`'s disk ends before `A`'s final checkpoint. | Copy the log again, as step 9 of [Prepare a release](#prepare-a-release) describes, and start `B` again. |

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
   with `signed snapshot payload is malformed` or `unsupported paired recovery format`, and an
   earlier release may not read the caches `B` wrote, such as one that carries unpaid withdrawals.
   If `A` stops with that error, move `/data/snapshots` aside and start it again: it replays the
   whole log and rebuilds its history index.

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
certificate is issued. Its configuration template is `config/ct-watch.toml`. Its `node.enclaves`
names the vault's `checkpointSigner()` and, with rotation on, each member's registered standby key;
it learns no key from any other enclave.

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

Staging is a second guest on the production host, on Arbitrum Sepolia with contracts of its own. Its
configuration is each release's `staging.toml`, which differs from `production.toml` only in its
chain, its contracts, accounts and providers, and its domain. Each release runs in staging for 24
hours before it goes to production. Staging's keyed market-data connections need vendor accounts of
their own, because Massive, TraderMade and Alpaca each allow one live connection per account.

1. **Set staging up once.** Run [Deploy the contracts](#deploy-the-contracts),
   [Deploy the rotation contracts](#deploy-the-rotation-contracts) and
   [First deployment](#first-deployment) against Arbitrum Sepolia, with the release's `staging.toml`
   as `node.toml`.
2. **Run each release in staging first.** Follow
   [Upgrade to a new release](#upgrade-to-a-new-release) on staging's guest and vault. Upgrade
   production only once the release has run in staging for 24 hours.
3. **Prove a rollback in staging** before you need one in production, with [Roll back](#roll-back).

Every step that changes staging needs its own approval, as the same step in production does. **An
approval for a step in staging does not cover that step in production.**
