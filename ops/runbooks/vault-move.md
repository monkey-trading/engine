# Move to a new vault

Use this runbook once, at public launch, to move the protocol from the alpha's vault to the launch
vault. The alpha is the deployment that runs before public launch, on a vault that holds mock USDC.
The launch vault holds real USDC. The move keeps MONKEY, its stakes and every bug bounty award, and
starts a new log on the launch vault whose genesis carries them.

The steps use the shell variables that [Variables](reference.md#variables) sets up, and the ones
that [Variables for the move](#variables-for-the-move) adds. Commands, configuration keys and output
call a running trading server a node. Every step marked **Changes the live system** needs its own
approval, and an approval for one step does not cover the next.

- [Know before you start](#know-before-you-start)
- [Variables for the move](#variables-for-the-move)
- [Timeline](#timeline)
- [Before the alpha deploys](#before-the-alpha-deploys)
- [Prepare the launch](#prepare-the-launch)
- [Stop the alpha](#stop-the-alpha)
- [Start the launch log](#start-the-launch-log)
- [Open to the public](#open-to-the-public)
- [The carried genesis holds or is refused](#the-carried-genesis-holds-or-is-refused)
- [Restore the alpha](#restore-the-alpha)
- [After the move](#after-the-move)

## Know before you start

- **What the launch log carries.** Its genesis carries, from the alpha's final accepted checkpoint,
  each account's MONKEY entitlement in full, each account's staked MONKEY with the point to which
  the alpha had mirrored the staking contract, every bug bounty award, and the emission schedule
  with its totals. The engine proves every carried value against the alpha's checkpoint root and
  checks that no entitlement, stake or award is left out.
- **What stays behind.** USDC balances, positions, working orders, withdrawal approvals, deposits,
  session keys, volume tiers, toxicity scores, elections, rotation sets, turns, pending votes and
  the awards' cap do not carry. The alpha's USDC stays on the alpha vault, and only the alpha
  vault's exit pays it, in mock USDC.
- **Staking distributions start at zero.** The launch vault pays no distribution that the alpha
  allocated, because the alpha paid its distributions in mock USDC.
- **Awards are carried unpaid.** An award that the alpha paid in mock USDC counts as unpaid. The
  launch log pays each carried award in full from its distributions, oldest first, under the launch
  cap on award payments.
- **MONKEY claims continue.** `MonkeyMinter` keeps one count of what each account has claimed across
  both vaults. An account that claimed part of its entitlement from the alpha claims the rest from
  the launch vault's checkpoints, and nothing is minted twice. Claims pause from the retirement of
  the alpha's signer in step 8 until the launch log's first checkpoint in step 11.
- **Emission periods before the launch log allocate nothing.** The period that the alpha was in at
  its final checkpoint, and every period until the launch log's first instruction after its genesis,
  allocate no MONKEY. Their budgets are cancelled. The halving schedule continues from the alpha's
  start.
- **Rotation pauses across the move.** From the alpha's last drawn turn until a rotation set adopted
  after the move has waited out its seven-day notice in the launch log, no standby can be promoted.
  Only the timelock's install fallback can replace a failed operator in that time. The public
  opening is therefore at least eight days after the launch log starts, and every launch trading
  server runs frozen until then, so that no client request is admitted. A deposit sent straight to
  the launch vault before the opening is credited, and its account can trade or withdraw from the
  opening on.
- **The alpha's log stays published for good.** The launch log's audit starts from the alpha's final
  checkpoint, so anyone who verifies the launch log needs the alpha's whole log.
- **Contracts.** `MonkeyToken`, `MonkeyMinter`, `Staking`, `OperatorRegistry`, the timelock,
  `SeedReceiver`, `SeedRelay` and `RegistryLens` are kept. The launch vault, with its exit verifier,
  and new `LogHeads`, `OperatorSchedule`, `ParameterVote` and `FaultProof` contracts are deployed
  for it. The alpha's contracts stay on chain. The console must show only the launch contracts.
- **The timelock's delay is three days.** From the first scheduling to the launch log's first
  instruction takes about six days.

## Variables for the move

Set these beside the variables in [Reference](reference.md#variables). `$VAULT`, `$LOG_HEADS` and
`$API` name the launch deployment from step 3 on.

```shell
export ALPHA_VAULT=0x...       # the alpha's vault: [genesis.carry] vault in the launch configuration
export ALPHA_SCHEDULE=0x...    # cast call $REGISTRY 'schedule()(address)' before step 10
export ALPHA_API=https://...   # the alpha's public API
export MINTER=0x...            # custody.evm.minter, kept across the move
export STAKING=0x...           # custody.evm.staking.contract, kept across the move
export SCHEDULE=0x...          # the launch schedule: custody.evm.staking.registry.schedule
export RECORD=0x...            # its governance record: cast call $SCHEDULE 'governanceRecord()(address)'
export VOTE=0x...              # the launch ParameterVote: custody.evm.staking.registry.vote.contract
export FAULT_PROOF=0x...       # the launch FaultProof: custody.evm.staking.registry.fault_proof.contract
export CANDIDATE=0x...         # the launch operator's candidate account
export ADMIN_KEY=0x...         # the launch operator's admin key: operator.public_key's address
export DIGEST=0x...            # the launch release's launch measurement
export CONFIG_HASH=0x...       # the launch release's configuration hash
```

## Timeline

T0 is the day the alpha stops taking deposits.

| When | Step |
| --- | --- |
| Before the alpha deploys | 1. Check that MONKEY's minter is `MonkeyMinter` |
| At least ten days before T0 | 2. Move the alpha to a release that runs V90 |
| By T0 − 3 days | 3. Deploy the launch contracts, 4. prepare the launch release, 5. schedule batches A, B and C |
| T0 | 6. Run A and C, boot the launch trading server, and schedule D |
| T0 to about T0 + 2 days | 7. Freeze the alpha, 8. retire its signer, 9. build the carry |
| Once the alpha's drawn turns end | 10. Run B |
| About T0 + 3 days | 11. Run D, 12. verify, 13. deposit house capital |
| After step 13 | 14. Turn rotation back on |
| At least eight days after step 11 | 15. Open to the public |

## Before the alpha deploys

### 1. Check that MONKEY's minter is `MonkeyMinter`

The move keeps MONKEY only if `MonkeyMinter` has been MONKEY's minter from the alpha's deployment
on. A token that another contract mints can never move to the launch vault.

```shell
export MONKEY=$(cast call $MINTER 'monkey()(address)' --rpc-url $RPC)
cast call $MONKEY 'minter()(address)' --rpc-url $RPC
cast call $STAKING 'monkey()(address)' --rpc-url $RPC
cast call $MINTER 'vault()(address)' --rpc-url $RPC
```

The first command prints `$MINTER`, the second prints `$MONKEY`, and the third prints
`$ALPHA_VAULT`.

**Approval:** the owner, as part of the alpha's deployment.

**If it fails:** if MONKEY's minter is any other address, do not deploy the alpha. The move could
not keep MONKEY.

## Prepare the launch

### 2. Move the alpha to a release that runs V90

The launch log can check that it carries every stake only if the alpha's root holds its total
staked, which protocol V90 adds. Every alpha member moves to a release whose build's
`ProtocolVersion::LATEST` is V90 or later, as [Before a new protocol version](operator-candidate.md#before-a-new-protocol-version)
and [Upgrade while you are the operator](operator-candidate.md#upgrade-while-you-are-the-operator)
describe. While votes govern, the activation waits for a passed vote and its seven-day notice, so
propose it, as [Parameter votes](operator-candidate.md#parameter-votes) describes, at least ten days
before T0.

```shell
curl -sf $ALPHA_API/v1/benchmarks | jq -r .protocol_version
```

The command prints `v90`, or a later version.

**Changes the live system.** The owner approves each release, and the multisig signs the timelock's
approvals.

**If it fails:** follow [Roll back](deploy-and-rollback.md#roll-back). Once the alpha's log has
activated V90, no build older than V90 can read it.

### 3. Deploy the launch contracts

From one deployer account, deploy in this order: `Vault` on real USDC, which deploys its exit
verifier; `LogHeads` for the launch vault; `OperatorSchedule` over the kept registry, the kept
`SeedReceiver` and the new `LogHeads`; `ParameterVote` over the new schedule; and `FaultProof` for
the launch vault. Plan the addresses with `cast compute-address`, and use the constructor values
that [Deploy the contracts](deploy-and-rollback.md#deploy-the-contracts) and
[Deploy the rotation contracts](deploy-and-rollback.md#deploy-the-rotation-contracts) give. The
launch vault's signer is a placeholder until step 11, and it must be the keyless `$PLACEHOLDER` that
[Deploy the contracts](deploy-and-rollback.md#deploy-the-contracts) derives: from batch B until D runs
in step 11, `MonkeyMinter` mints against the checkpoints that the launch vault's signer posts, and until
D a key for that signer could also pay itself any USDC deposited to the launch vault. Do not set the
launch vault's registry here, because batch B sets it after the registry has moved.

Check the pairing that `setCustody` requires:

```shell
cast call $SCHEDULE 'registry()(address)' --rpc-url $RPC
cast call $SCHEDULE 'logHeads()(address)' --rpc-url $RPC
cast call $LOG_HEADS 'vault()(address)' --rpc-url $RPC
```

The commands print `$REGISTRY`, `$LOG_HEADS` and `$VAULT`. Check the governance record the schedule
created, which batch C fills:

```shell
cast call $SCHEDULE 'governanceRecord()(address)' --rpc-url $RPC
cast call $RECORD 'schedule()(address)' --rpc-url $RPC
cast call $RECORD 'vault()(address)' --rpc-url $RPC
```

The commands print `$RECORD`, `$SCHEDULE` and `$VAULT`. Check the launch vault's signer too:

```shell
cast call $VAULT 'checkpointSigner()(address)' --rpc-url $RPC
```

The command prints `$PLACEHOLDER`. If it prints any other address, deploy the launch vault again.

**Changes the live system.** The owner approves each deployment.

**If it fails:** deploy the contract again. Nothing refers to these contracts yet.

### 4. Prepare the launch release

Prepare the launch release no later than T0 − 3 days, as [Prepare a release](deploy-and-rollback.md#prepare-a-release)
describes. Its configuration names the launch contracts from step 3 and the kept registry, staking
contract, minter and seed receiver. Its genesis initialises staking in `[genesis.staking]`, and it
carries the alpha's state:

```toml
[genesis.carry]
vault = "0x..."            # $ALPHA_VAULT
file = "/data/carry.json"  # the carry file that step 9 writes
```

Set `[genesis.staking]`'s `start_ms` after the latest time the launch log's genesis can run: step 11's
D, plus a margin that covers step 8's three-day fallback. The engine refuses a staking start before the
genesis's own stamp, and the start refuses with `carry_refused`, before anything is written, a carry it
would run after `start_ms`.

The configuration does not pin the carry file by hash. The launch log's own checks fix the carried
values from the alpha's root, and the trading server reads that root from the alpha vault, so the
timelock can approve the release before the alpha freezes. Ship the console release that pins the
launch release, and shows only the launch contracts.

**Approval:** the owner.

**If it fails:** prepare the release again.

### 5. Schedule batches A, B and C

At T0 − 3 days, schedule three batches, as
[Calling several contracts in one timelock batch](reference.md#calling-several-contracts-in-one-timelock-batch)
describes, each with a salt of its own. Each batch can be cancelled until it runs. A batch that
reverts on a condition stays ready, and runs once the condition holds.

Batch A stops the alpha's deposits and turns rotation off:

```shell
TARGETS="[$ALPHA_VAULT,$REGISTRY]"
VALUES="[0,0]"
PAYLOADS="[$(cast calldata 'setPauses(bool,bool)' true false),$(cast calldata 'setRotation(bool)' false)]"
```

Batch B moves custody, minting and the operator to the launch vault, in this order. `setCustody`
reverts with `TurnsPending` until the alpha's drawn turns have ended, and `setVault` on the minter
reverts with `SignerLive` until the alpha's signer is retired.

```shell
TARGETS="[$REGISTRY,$VAULT,$MINTER,$REGISTRY]"
VALUES="[0,0,0,0]"
PAYLOADS="[$(cast calldata 'setCustody(address,address)' $VAULT $SCHEDULE),$(cast calldata 'setRegistry(address)' $REGISTRY),$(cast calldata 'setVault(address)' $VAULT),$(cast calldata 'installOperator(address,address,bytes,bytes32)' $CANDIDATE $ADMIN_KEY $DIGEST $CONFIG_HASH)]"
```

Batch C names the launch release on the launch vault, and records the launch `ParameterVote` and
`FaultProof` in the launch schedule's governance record, once and for good, as step 7 of
[Deploy the rotation contracts](deploy-and-rollback.md#deploy-the-rotation-contracts) describes:

```shell
TARGETS="[$VAULT,$RECORD]"
VALUES="[0,0]"
PAYLOADS="[$(cast calldata 'setCurrentRelease(bytes,bytes32)' $DIGEST $CONFIG_HASH),$(cast calldata 'setGovernanceContracts(address,address)' $VOTE $FAULT_PROOF)]"
```

**Changes the live system.** The owner approves each batch, and the multisig signs it.

**If it fails:** cancel the batch and schedule it again. Nothing has run.

## Stop the alpha

### 6. Run A and C, boot the launch trading server, and schedule D

At T0, run batches A and C.

**Changes the live system.** The owner approves each execution.

```shell
cast call $ALPHA_VAULT 'depositsPaused()(bool)' --rpc-url $RPC
cast call $REGISTRY 'rotationSwitch()(bool)' --rpc-url $RPC
cast call $VAULT 'currentRelease()(bytes32)' --rpc-url $RPC
cast call $RECORD 'vote()(address)' --rpc-url $RPC
cast call $RECORD 'faultProof()(address)' --rpc-url $RPC
```

The first command prints `true`, the second prints `false`, the third prints the launch release's
id, `cast keccak $(cast concat-hex $DIGEST $CONFIG_HASH)`, and the last two print `$VOTE` and
`$FAULT_PROOF`. The alpha mirrors rotation's
switch once the block is `finalized`. The turns already drawn keep naming the alpha's operator for
about two more days. The alpha's checkpoints, withdrawals and claims continue.

Boot the launch trading server with new keys, deliver its secrets and verify its attestation, as
steps 2 to 4 of [First deployment](deploy-and-rollback.md#first-deployment) describe. It serves only
its attestation, and its serial console shows `signer_unregistered`. Then schedule D, the launch
vault's `setSigner($SIGNER)`, as
[Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock)
describes.

**Changes the live system.** The owner approves the boot, and the owner and the multisig approve D.

**If it fails:** boot the launch trading server again. D can be cancelled until it runs.

### 7. Freeze the alpha

Freeze the alpha within its last drawn turn. Find when the drawn turns end, and the trading server
that holds the turn now:

```shell
cast call $ALPHA_SCHEDULE 'turnsRunUntil()(uint64)' --rpc-url $RPC
curl -sf $ALPHA_API/v1/operator | jq .
```

First put an empty file named `frozen` on the data disk of every other alpha member, and restart
each: a member that is promoted or skipped to while the turn's trading server restarts then serves
frozen too. Then restart the turn's trading server with `engine run --frozen`: stop its virtual
machine, put the `frozen` file on its data disk, and start it again. A frozen trading server admits no
client request. It keeps stamping prices, mirroring the chain and posting checkpoints on its usual
pacing, and its reads and stream are unchanged.

**Changes the live system.** The owner approves the restart.

```shell
curl -s -o /dev/null -w '%{http_code}\n' -X POST $ALPHA_API/v1/orders
```

The command prints `503`. If it prints anything else, the trading server serving the alpha's API
is not frozen: find it with `/v1/operator`, put the `frozen` file on its data disk and restart it.

Then check, after each checkpoint the alpha vault accepts, whether a carry from it would pass. Run
the check on a copy of the alpha's log that holds the alpha vault's newest checkpoint, such as a
following member's copy, taken as [Running as a standby](standby.md) shows. The archive does not
serve the segment still being written. With `archive.local_window`, as the alpha's release sets, the
copy lacks its oldest segments: each `engine carry` of it below takes `--archive-config <alpha config>`,
the alpha node's configuration, and the `engine replay` with `--carry-source` takes
`--carry-source-archive-config <alpha config>`, either of which puts them back into the copy from the
alpha's bucket first, so the copy needs room for the whole log
([Keep a window of the log on the host](log-archive.md#keep-a-window-of-the-log-on-the-host)).

```shell
S=$(cast call $ALPHA_VAULT 'latestSeq()(uint64)' --rpc-url $RPC)
$ENGINE carry --log <alpha log copy> --seq $S --check --archive-config <alpha config>
```

`<alpha log copy>` is that copy's log directory. The command exits with status 0 when a carry from
the checkpoint at `$S` would pass. Otherwise it exits non-zero and names the first refusal:

- **Results in the open emission period.** A working order, a take profit, a stop loss or a
  close-out was executed in the current ten-minute period. Check again after the next accepted
  checkpoint.
- **Rotation is on.** Batch A's block is not yet `finalized`, or the alpha has not yet mirrored it.
  Check again after the next accepted checkpoint.
- **The staking mirror's frontier misses a block.** The alpha has mirrored an event from a block
  that its completed scan has not passed. Check again after the next accepted checkpoint.
- **An award proposal waits to apply.** A carry would lose it, so the check refuses until the award
  applies, seven days after the alpha logged its vote. Keep the alpha frozen and running, and check
  again after the next checkpoint the alpha vault accepts once the award has applied. If the alpha's
  drawn turns end first, install the alpha's operator again, as **If it fails** describes.
- **The alpha is before V90.** Step 2 is not done. Stop, and finish step 2.

**If it fails:** restart the alpha's trading server without the `frozen` file. Nothing on chain has
changed. If the drawn turns run out before the alpha freezes, install the alpha's operator again
with a release whose configuration sets `operator.rotation_enabled = false`, with the timelock's
fallback, as [Rotation off and failed operators](operator-candidate.md#rotation-off-and-failed-operators)
describes.

### 8. Retire the alpha's signer

Once the check in step 7 passes, stop the virtual machine of the alpha's trading server, and of
every member, so that no checkpoint past the one checked can reach the vault. A checkpoint posted
just before the stop can still land, so wait until the alpha's checkpoint relayer,
`checkpoints.relayer` in its configuration, has nothing pending, and the vault's latest checkpoint
is `finalized`:

```shell
cast nonce <relayer> --block pending --rpc-url $RPC
cast nonce <relayer> --block latest --rpc-url $RPC
S=$(cast call $ALPHA_VAULT 'latestSeq()(uint64)' --block finalized --rpc-url $RPC)
cast call $ALPHA_VAULT 'latestSeq()(uint64)' --rpc-url $RPC
$ENGINE carry --log <alpha log copy> --seq $S --check --archive-config <alpha config>
```

The first two commands print the same number, and the next two the same sequence number, `$S`. The
check runs at `$S` as step 7's does, on a copy that holds that checkpoint, such as the stopped trading
server's log. If it fails, start the alpha's trading servers again, frozen, and check again after the
next accepted checkpoint, as step 7 describes. Nothing on chain has changed. Once the alpha's drawn
turns have ended, its trading servers start again only as [Restore the alpha](#restore-the-alpha)
describes, so leave time for this within the last turn.

Once the check passes at `$S`, the guardian retires the alpha's signer.

**Changes the live system.** The owner and the guardian multisig approve the retirement.

```shell
cast send $ALPHA_VAULT 'retireSigner()' --rpc-url $RPC --account <guardian>
cast call $ALPHA_VAULT 'checkpointSigner()(address)' --rpc-url $RPC
cast call $ALPHA_VAULT 'latestSeq()(uint64)' --rpc-url $RPC
```

The second command prints the zero address, and the third prints `$S`, the alpha's final checkpoint,
from which the launch log carries. The alpha vault now refuses checkpoints, withdrawals and claims,
and MONKEY claims pause until step 11.

**If it fails:** if the guardian has not retired the signer, start the alpha's trading servers
again without the `frozen` file, which undoes step 7. If the move is abandoned once the signer is retired, follow
[Restore the alpha](#restore-the-alpha). If the retirement is delayed past the
`[genesis.staking]` start's margin that step 4 set, the launch release must be prepared again with a
later start, or its genesis is refused.

### 9. Build the carry

Build the carry file from the alpha's log at `$S`:

```shell
$ENGINE carry --log <alpha log copy> --seq $S --out carry.json --archive-config <alpha config>
```

The command replays the log to its checkpoint at `$S`, checks the checkpoint's root, writes
`carry.json`, and prints the totals and the file's SHA-256. It refuses to overwrite `carry.json`, and
refuses a carry over 48 MiB, which the launch log's first segment could not hold for the members
that fetch it from the archive.
The totals name the accounts that hold an entitlement or a stake, the MONKEY entitled, the MONKEY
staked and the number of awards. The owner reviews them, and the file and its totals are published
with the launch, so that anyone can see what each alpha account carries:

```shell
jq -r '.parts[] | select(.part == "entitlements") | .entries[] | "\(.account) \(.amount)"' carry.json > entitlements.txt
jq -r '.parts[] | select(.part == "stakes") | .entries[] | "\(.account) \(.amount)"' carry.json > stakes.txt
```

Then put the file where the launch configuration's `[genesis.carry] file` names it: stop the launch
trading server's virtual machine, copy `carry.json` to `/data/carry.json` on its data disk, put an
empty file named `frozen` beside it, and start it again without a `new-key` file. It opens the keys
that it sealed in step 6, and it still serves only its attestation. The `frozen` file stays until
step 15.

**Changes the live system.** The owner approves the totals, and the restart.

**If it fails:** if the tool refuses, follow [Restore the alpha](#restore-the-alpha).

## Start the launch log

### 10. Run B

Run batch B once the alpha's drawn turns have ended and its signer is retired:

```shell
cast call $ALPHA_SCHEDULE 'turnsRunUntil()(uint64)' --rpc-url $RPC
cast block latest --field timestamp --rpc-url $RPC
cast call $ALPHA_VAULT 'checkpointSigner()(address)' --rpc-url $RPC
```

The first value is no later than the second, and the third command prints the zero address.

**Changes the live system.** The owner approved the execution when the batch was scheduled.

```shell
cast call $REGISTRY 'vault()(address)' --rpc-url $RPC
cast call $REGISTRY 'schedule()(address)' --rpc-url $RPC
cast call $MINTER 'vault()(address)' --rpc-url $RPC
cast call $REGISTRY 'operatorInEffect()((address,address,bytes32,uint64))' --rpc-url $RPC
```

The commands print `$VAULT`, `$SCHEDULE`, `$VAULT`, and the launch operator with `$ADMIN_KEY`.

**If it fails:** a batch that reverted on a condition runs again once the condition holds. To undo
B before step 11, follow [Restore the alpha](#restore-the-alpha).

### 11. Run D

At about T0 + 3 days, run D. Once its block is `finalized`, the launch trading server is
registered. Before its genesis, it reads the chain at a `finalized` block through the custody
providers, and holds under the Warning `carry_waiting` until all of these hold:

- the alpha vault's signer is retired, and it holds a checkpoint;
- the registry follows the launch vault, and rotation's switch is off;
- the registry's operator in effect has the configuration's operator key;
- `MonkeyMinter` follows the launch vault;
- the carry file is at `[genesis.carry] file`.

At every read, before it waits, it checks that the configuration's registry is the one the alpha vault reads, that
the configuration's staking contract is the one that registry deployed, whose events the alpha mirrored, and that
the minter mints the token the staking contract holds, so a wrong address refuses at once instead of waiting. Once every condition holds it checks that the carry file's
source is the alpha vault's `latestSeq` and its root there, that the genesis with the carry applies
in full, and that the carried emission budget matches the minter's cap. It sequences its genesis with the carry, and then its first checkpoint. Carried MONKEY
is claimable from that checkpoint.

**Changes the live system.** The owner approved the execution when D was scheduled.

```shell
curl -sf $API/v1/benchmarks | jq -r .protocol_version
curl -sf $API/v1/monkey | jq .emissions
curl -sf $API/v1/awards | jq .
```

The first command prints `v92`, or a later version: from V92 the launch log refuses a staking or registry event at or below the carried frontier. The second shows the alpha's emission schedule. The third lists the
carried awards, with nothing paid.

**If it fails:** follow [The carried genesis holds or is refused](#the-carried-genesis-holds-or-is-refused).
Before any deposit, the launch log can be abandoned and started fresh.

### 12. Verify the move

These checks only read.

1. Replay the launch log against the alpha's. Download the launch log as
   [Download the public log and verify it](reference.md#download-the-public-log-and-verify-it)
   shows, into `launch-archive`, and use the alpha log copy from step 9:

   ```shell
   $ENGINE replay --log launch-archive --carry-source <alpha log copy> --carry-source-archive-config <alpha config>
   ```

   After the usual replay, the command replays the alpha's log to the carried sequence number. It
   requires the alpha's checkpoint there to hold the carried root, signed by a checkpoint signer
   that the alpha's log names. It requires the launch log's carried entitlements, stakes, emissions
   and staking frontier, and its awards with nothing paid, to equal the alpha's state there, and the
   replay to have applied every part. It passes only when it exits with status 0 and its last line
   begins `carried from:`; any other exit is a failure, whatever it printed before.

   The replay ties the carried root to the alpha's signed checkpoint, not to the chain. Compare it
   with the alpha vault's final checkpoint, `<seq>` being the sequence number that line names:

   ```shell
   cast call $ALPHA_VAULT 'latestSeq()(uint64)' --rpc-url $RPC
   cast call $ALPHA_VAULT 'checkpointRoots(uint64)(bytes32)' <seq> --rpc-url $RPC
   ```

   The first command prints `<seq>`, and the second prints the root that the line names.

2. Check a carried account's claim. When a carried account claims, read what the minter has minted
   to it:

   ```shell
   curl -sf $API/v1/accounts/<account>/emissions | jq -r .entitled
   cast call $MINTER 'emissionsClaimed(address)(uint256)' <account> --rpc-url $RPC
   ```

   `<account>` is an account in `entitlements.txt`. After its claim, the two values are equal.

3. Check each carried stake against the staking contract at the block to which the alpha had
   mirrored it. A staker's MONKEY counts in the engine until its unstake completes, so the engine's
   stake is the contract's `stakes` plus the amount of its pending unstake. The reads need an RPC
   provider that serves state at past blocks, and `bc`:

   ```shell
   B=$(jq -r '.parts[0].globals | fromjson | .staking.completed_through.block' carry.json)
   while read -r account amount; do
     staked=$(cast call $STAKING 'stakes(address)(uint256)' $account --block $B --rpc-url $RPC | cut -d' ' -f1)
     pending=$(cast call $STAKING 'unstakes(address)(uint256,uint256)' $account --block $B --rpc-url $RPC | head -1 | cut -d' ' -f1)
     [ "$(echo "$staked + $pending" | bc)" = "$amount" ] || echo "differs: $account"
   done < stakes.txt
   ```

   The loop prints nothing.

Publish the results with the carry file.

### 13. Deposit house capital

Deposit the house's capital in real USDC with `depositToHouse`, as step 11 of
[First deployment](deploy-and-rollback.md#first-deployment) describes.

**Changes the live system.** The owner approves the deposit.

## Open to the public

### 14. Turn rotation back on

Members register standby keys with the launch schedule, as
[Register your standby key](operator-candidate.md#register-your-standby-key) describes, and the
timelock approves each member's launch release. Each member's configuration names the launch
operator's `[genesis.carry]` `vault`, as [The candidate's configuration](operator-candidate.md#the-candidates-configuration)
requires, and so does every later release of the launch log, an upgrade's included: a member whose
configuration does not raises `carry_refused`, registers no standby key, and stops at its turn
before it takes the keys, and an upgrade's release stops before the handover completes. A member
needs no carry file. Each member's data disk holds a `frozen` file, as
the operator's does, so that a member promoted or taking a turn before step 15 serves frozen too. Check the release condition in
[Register the incumbent before rotation is turned on](operator-candidate.md#register-the-incumbent-before-rotation-is-turned-on),
and then the timelock calls `setRotation(true)` on the registry. The next election close adopts a
rotation set, and its turns run in the launch log once its seven-day notice has passed.

**Changes the live system.** The owner approves the call, and the multisig signs it.

### 15. Open to the public

Open to the public at least eight days after the launch log's first instruction, once a rotation
set adopted after the move has waited out its notice and its turns run in the launch log.

Remove the `frozen` file from the data disk of every launch member but the one that holds the turn,
and restart each. Then do the same for the turn's trading server, which `/v1/operator` names. Until
then, check that the launch log admits no client request:

```shell
curl -s -o /dev/null -w '%{http_code}\n' -X POST $API/v1/orders
```

The command prints `503` before the opening, and any other code once the turn's trading server has
restarted without the `frozen` file. Then announce the launch vault, and open deposits in the
console.

**Changes the live system.** The owner approves the opening and each restart.

**If it fails:** a trading server that still answers `503` holds the `frozen` file. Remove it and
restart that trading server.

## The carried genesis holds or is refused

Use this section when `carry_waiting` or `carry_refused` fires on the launch trading server.

- **`carry_waiting`** is a Warning. The trading server holds before its genesis, and the message
  names each condition from step 11 that does not yet hold. Nothing is written. A condition that
  waits on batch B, on the retirement of the alpha's signer or on the carry file resolves when you
  finish the step that it names. The alert resolves by itself once every condition holds.
- **`carry_refused`** is Critical. The configured registry is not the one the alpha vault reads,
  the configured staking contract is not the one that registry deployed, the configured minter mints
  another token than the staking contract holds, the carry file's source is not the alpha vault's final checkpoint, the file does
  not parse or its totals are not what its parts carry, the genesis with the carry does not apply in
  full at the start's time or at the stamp it takes, or the carried emission budget differs from the
  minter's cap. Nothing is written, and the trading server stops. A refusal at the stamp, such as a
  `[genesis.staking]` start that the hold ran past, needs a new release, as step 4 describes. Build the file again from the alpha vault's `latestSeq` with step 9, put it on the data
  disk, and start the trading server again. If the configuration is wrong, prepare a new release,
  which takes the timelock's three-day delay. Before any deposit, you may instead abandon the log:
  with the trading server stopped, remove `node.log_dir` and `history.dir`, and start again.

A trading server whose log has carried a genesis refuses to start under a configuration that names
another source vault, or another chain. A configuration that names `[genesis.carry]` likewise
refuses a log whose genesis carried nothing. Start it under the configuration that the log was
started with, as [The trading server will not start](restart-and-recovery.md#the-trading-server-will-not-start)
describes.

## Restore the alpha

Use this section, before the launch log's genesis in step 11, when the alpha must start again once
the guardian has retired its signer in step 8, or once its drawn turns have ended. It gives the
alpha back its operator, its release and a signer, and takes the timelock's delay twice. Start at once: anyone may start the alpha
vault's exit fourteen days after its last checkpoint.

Once the alpha's drawn turns end, the alpha vault names no release, and the last turn's member stays
named, until the timelock installs an operator and names a release, as
[When no standby qualifies](standby.md#when-no-standby-qualifies) describes. Its trading servers can
open no keys until then. `<alpha candidate>` and `<alpha admin key>` are the candidate and admin key
of the release that the alpha ran in step 7.

The restored alpha runs a release of its own configuration with `operator.rotation_enabled = false`,
prepared as [Prepare a release](deploy-and-rollback.md#prepare-a-release) describes: once the
installation has ended the drawn turns, a trading server whose configuration turns rotation on
refuses to start. `<alpha digest>` is that release's launch measurement, and `<alpha config hash>`
its configuration hash.

1. If batch B ran, the guardian retires the launch vault's placeholder signer, which `setCustody`
   and `setVault` require before they leave the launch vault:

   ```shell
   cast send $VAULT 'retireSigner()' --rpc-url $RPC --account <guardian>
   ```

   **Changes the live system.** The owner and the guardian multisig approve the retirement.

2. Schedule one batch with the pattern in
   [Calling several contracts in one timelock batch](reference.md#calling-several-contracts-in-one-timelock-batch).
   If batch B ran, it calls these, in this order:

   ```shell
   TARGETS="[$REGISTRY,$REGISTRY,$ALPHA_VAULT,$MINTER]"
   VALUES="[0,0,0,0]"
   PAYLOADS="[$(cast calldata 'setCustody(address,address)' $ALPHA_VAULT $ALPHA_SCHEDULE),$(cast calldata \
     'installOperator(address,address,bytes,bytes32)' <alpha candidate> <alpha admin key> <alpha digest> \
     <alpha config hash>),$(cast calldata 'setCurrentRelease(bytes,bytes32)' <alpha digest> <alpha config hash>),$(cast \
     calldata 'setVault(address)' $ALPHA_VAULT)]"
   ```

   If B has not run, leave out the first and the last call. Execute the batch once the alpha's drawn
   turns have ended: before then, `installOperator` reverts with `TurnsPending`.

   **Changes the live system.** The owner approves the batch, and the multisig signs it.

   ```shell
   cast call $REGISTRY 'operatorInEffect()((address,address,bytes32,uint64))' --rpc-url $RPC
   cast call $ALPHA_VAULT 'currentRelease()(bytes32)' --rpc-url $RPC
   cast call $MINTER 'vault()(address)' --rpc-url $RPC
   ```

   The first command names `<alpha admin key>`, and its third field equals the second command's
   output. The third command prints `$ALPHA_VAULT`.

3. The alpha's restored release runs in a new generation. Boot its trading server with `new-key`,
   verify its attestation, and have the timelock call `setSigner` on the alpha vault with its new
   signer, as step 4 of [When no standby qualifies](standby.md#when-no-standby-qualifies) describes.
   Its trading servers start without the `frozen` file, which undoes step 7. Checkpoints, withdrawals
   and claims resume on the alpha vault in a new signer epoch.

   **Changes the live system.** The owner approves the boot and the contract call.

## After the move

- **Keep the alpha's log published.** Publish its whole log, the active segment included, once the
  alpha's trading server stops, and keep it in at least two stores. Anyone who verifies the launch
  log replays it with `--carry-source`.
- **Keep the alpha's last segments.** The alpha's log stops at the move, so its last range of 10,000
  sequence numbers never fills and never goes up to the archive's bucket as a range. Its commands
  through the alpha's final checkpoint are in the bucket in that checkpoint's tail, from which
  `engine replay` and `engine exit-proof` read the log through the vault's latest checkpoint; the
  segment files after the last whole range, and any command after the final checkpoint, are only on
  the alpha's hosts and in the copies you publish. With `archive.local_window`, a host never removes a
  segment that holds a sequence number of a range the bucket does not hold, tails aside, so each alpha
  host keeps them however old they get. Keep at least one alpha host's log directory, or a checked copy
  of those segments with the alpha's `manifest.jsonl`.
- **The alpha vault's exit stays open to anyone.** Fourteen days after the alpha's final
  checkpoint, anyone may start the alpha vault's exit, which pays mock USDC only. It cannot mint
  MONKEY, and nothing it pays reaches the launch vault.
- **The alpha's contracts stay callable.** Once rotation is on, anyone can draw turns on the alpha's
  schedule, and its rotation set can vote on the alpha's `ParameterVote`. The launch trading server
  mirrors neither.
