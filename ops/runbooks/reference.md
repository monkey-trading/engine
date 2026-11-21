# Reference

This page sets up the tools, shell variables and checks that the other runbooks use. Read it once
before you follow any runbook, and keep open the shell in which you set the variables. The trading
server runs inside an enclave, and commands, configuration keys and output call a running trading
server a node.

- [Tools on the administrative workstation](#tools-on-the-administrative-workstation)
- [Variables](#variables)
- [The trading server's attested key and its pin](#the-trading-servers-attested-key-and-its-pin)
- [Health in one minute](#health-in-one-minute)
- [Read the vault and the registry](#read-the-vault-and-the-registry)
- [Calling a contract through the timelock](#calling-a-contract-through-the-timelock)
- [Calling several contracts in one timelock batch](#calling-several-contracts-in-one-timelock-batch)
- [Chain finality](#chain-finality)
- [Deadlines that run while the trading server is down](#deadlines-that-run-while-the-trading-server-is-down)
- [Download the public log and verify it](#download-the-public-log-and-verify-it)
- [Where things live on the host](#where-things-live-on-the-host)

## Tools on the administrative workstation

You need these tools on the workstation from which you run the runbooks' commands:

- **`engine`,** the trading server's binary, built from the release tag with the production feature
  so that its configuration checks match the trading server's:

  ```shell
  cargo build --release -p engine-node --features production
  ```

  The binary is written to `target/release/engine`. It provides `admin`, `replay`,
  `verify-attestation`, `verify-response`, `verify-frames`, `exit-proof` and `ct-watch`, and
  `engine <command> --help` lists each command's flags.
- **`cast`,** from Foundry, to read the contracts and to send the transactions that you are approved
  to send.
- **`curl`, `jq`, `xxd` and `openssl`.**

**Never put a private key on a command line or in your shell history.** Use a hardware wallet, a
Foundry keystore with `--account <name>`, or, for the operator's administrative key, a file that
only you can read, passed with `--key-file`.

## Variables

Set these variables in the shell you will run the runbooks from. Each comment names the
configuration key or the call that holds the value.

```shell
export API=https://api.example.com     # the trading server's public API: api.tls.acme.domain
export ENGINE=./engine                  # the binary built above
export CONFIG=node.toml                 # the configuration the running release was started with
export VAULT=0x...                      # custody.vault
export LOG_HEADS=0x...                  # log_heads.contract, if configured
export REGISTRY=0x...                   # custody.evm.staking.registry.contract
export USDC=0x...                       # custody.evm.usdc
export TIMELOCK=0x...                   # the vault's governance(): cast call $VAULT 'governance()(address)'
export RPC=https://...                  # one custody provider, for these reads
export SECRETS=host:port                # the delivery listener, as the host forwards tee.sealing.secrets.listen
```

Check the chain and the vault:

```shell
cast chain-id --rpc-url $RPC
cast call $VAULT 'usdc()(address)' --rpc-url $RPC
```

The first command prints `42161`, which is Arbitrum One. The second prints the USDC address that
`custody.evm.usdc` names.

## The trading server's attested key and its pin

Every `engine admin` call is pinned to the trading server's TLS key, so the call fails if anything
other than the attested enclave answers. Take the pin from an attestation that you have verified.

1. Download the attestation and verify it:

   ```shell
   curl -sf $API/v1/attestation > attestation.json
   $ENGINE verify-attestation --attestation attestation.json --release release.toml --processor <processor>
   ```

   `release.toml` is the manifest published with the release that the trading server runs, as
   [Deploy and rollback](deploy-and-rollback.md) describes. `<processor>` is the codename of the
   host's CPU generation, `Genoa` or `Milan`. The command exits with status 0 and prints a line that
   begins `verified:`.

2. Set the pin and the signer from the verified attestation:

   ```shell
   BUNDLE=$(jq -r '.bundle[2:]' attestation.json)
   export PIN=${BUNDLE: -64}
   export SIGNER=$(jq -r .enclave_address attestation.json)
   ```

   `$PIN` is bytes 73 to 105 of the bundle, which are the SHA-256 hash of the TLS key. It equals the
   output of `jq -r '.tls_public_key_sha256[2:]' attestation.json`.

With `[tee.sealing]`, the TLS key survives restarts and upgrades, so the pin changes only when new
keys are made, as [Key recovery](key-recovery.md) describes.

Once `OPERATOR_KEY_FILE` names a file that holds the operator's key, an administrative call has this
form:

```shell
$ENGINE admin --config $CONFIG --node $API --pin $PIN <command>
```

`--pin` goes before the command. The configuration must be the one that the trading server runs,
because it names the operator's key, and the chain and vault for which the instruction is signed.

## Health in one minute

Use these checks to see the state of the trading server at a glance.

```shell
curl -sf $API/v1/health
curl -sf $API/v1/operator | jq .
curl -sf $API/v1/markets | jq -c '.[] | {id, symbol, open, reduce_only, mark, stress_level}'
curl -s http://<metrics host>:9464/metrics | grep -E '^engine_(withdrawals_paused|market_priced|last_checkpoint_timestamp_seconds|alert_pages_pending|sequencer_queue_depth|tls_certificate_pending) '
```

`<metrics host>` is the address of the trading server's metrics listener. The first command prints
`ok`. The second shows the operator in effect, any pending rotation, and the last election. The
third shows each market's state. Read the metrics as follows:

- `engine_withdrawals_paused 0` means that withdrawal authorisations are being signed. A value of 1
  means that they are paused, and [Withdrawal stalls](withdrawal-stalls.md) says why.
- `engine_last_checkpoint_timestamp_seconds` is the sequenced time of the instruction that the last
  signed checkpoint covers. It trails the clock while a checkpoint is held back, waiting for the
  previous one to become final.
- `engine_alert_pages_pending` above 0 for long means that pages are not being delivered.
- Every market that has an open session and a feed should read `engine_market_priced` 1. A market
  that reads 0 while `open` is `true` has no price, as
  [A market has no price](feed-and-custody-failures.md#a-market-has-no-price-market_no_price)
  describes.

**The metrics listener is plain HTTP.** It carries no data about individual accounts. Reach it only
from the monitoring network.

## Read the vault and the registry

These calls read the contracts' state at the latest block:

```shell
cast call $VAULT 'latestSeq()(uint64)'            --rpc-url $RPC   # sequence number of the last posted checkpoint
cast call $VAULT 'latestRoot()(bytes32)'          --rpc-url $RPC
cast call $VAULT 'lastCheckpointAt()(uint64)'     --rpc-url $RPC   # block time of the last checkpoint
cast call $VAULT 'checkpointSigner()(address)'    --rpc-url $RPC   # the zero address means the guardian retired it
cast call $VAULT 'signerEpoch()(uint64)'          --rpc-url $RPC
cast call $VAULT 'currentRelease()(bytes32)'      --rpc-url $RPC
cast call $VAULT 'releaseGeneration()(uint64)'    --rpc-url $RPC
cast call $VAULT 'withdrawalsPaused()(bool)'      --rpc-url $RPC
cast call $VAULT 'depositsPaused()(bool)'         --rpc-url $RPC
cast call $VAULT 'checkpointsStale()(bool)'       --rpc-url $RPC
cast call $REGISTRY 'rotationEnabled()(bool)'     --rpc-url $RPC
cast call $REGISTRY 'operatorInEffect()((address,address,bytes32,uint64))' --rpc-url $RPC   # candidate, admin key, release, since
cast call $REGISTRY 'pendingElection()((uint64,address,address,bytes32,uint64,bool))' --rpc-url $RPC
```

The trading server acts on the `finalized` block, not the latest one. A change that you have just
sent therefore reaches the trading server only after the delay that
[Chain finality](#chain-finality) describes.

## Calling a contract through the timelock

Every governance call in these runbooks runs through the vault's timelock. These calls include
`setCurrentRelease`, `setSigner`, `setReleaseApproval`, `setRotation` and `setPauses`. The proposer
schedules the call, the delay passes, and the executor runs it. Each runbook gives the call's target
and data, and this procedure gives the steps.

1. Prepare the call:

   ```shell
   TARGET=$VAULT
   DATA=$(cast calldata '<function signature>' <arguments>)
   ZERO=0x0000000000000000000000000000000000000000000000000000000000000000
   SALT=$(cast keccak "<label>")
   DELAY=$(cast call $TIMELOCK 'getMinDelay()(uint256)' --rpc-url $RPC)
   ```

   Set `TARGET` to `$REGISTRY` instead for a call to the registry. `<function signature>` and
   `<arguments>` come from the runbook. `<label>` is any text that no earlier call has used as its
   salt.

2. Have the proposer schedule the call. When the proposer is a multisig, submit this call as a
   multisig transaction.

   **Changes the live system.** The owner must approve the call, and the multisig must sign it.

   ```shell
   cast send $TIMELOCK 'schedule(address,uint256,bytes,bytes32,bytes32,uint256)' $TARGET 0 $DATA $ZERO $SALT $DELAY --rpc-url $RPC --account <proposer>
   ```

3. After the delay, check that the call is ready:

   ```shell
   OP=$(cast call $TIMELOCK 'hashOperation(address,uint256,bytes,bytes32,bytes32)(bytes32)' $TARGET 0 $DATA $ZERO $SALT --rpc-url $RPC)
   cast call $TIMELOCK 'isOperationReady(bytes32)(bool)' $OP --rpc-url $RPC
   ```

   The second command prints `true`.

4. Have the executor run the call:

   **Changes the live system.** The owner must approve the execution, which can be a separate yes
   from the approval to schedule the call.

   ```shell
   cast send $TIMELOCK 'execute(address,uint256,bytes,bytes32,bytes32)' $TARGET 0 $DATA $ZERO $SALT --rpc-url $RPC --account <executor>
   ```

   If the call reverts, `cast` prints the contract's error name. The runbook for the call lists the
   errors to expect.

5. Confirm that the call has run:

   ```shell
   cast call $TIMELOCK 'isOperationDone(bytes32)(bool)' $OP --rpc-url $RPC
   ```

   The command prints `true`. Read the call's effect from the contract, with the read that the
   runbook names. The trading server acts on the call only once the block that holds it is
   `finalized`.

To withdraw a scheduled call before it runs, the proposer cancels it.

**Changes the live system.** The owner must approve the cancellation.

```shell
cast send $TIMELOCK 'cancel(bytes32)' $OP --rpc-url $RPC --account <proposer>
```

## Calling several contracts in one timelock batch

Some runbooks need calls that take effect together, in one transaction and in a fixed order. The
timelock's batch runs the calls in the order listed. If any call reverts, the whole batch reverts
and nothing changes, and the batch stays ready to run again. The steps follow
[Calling a contract through the timelock](#calling-a-contract-through-the-timelock).

1. Prepare the calls, in the order that the runbook gives:

   ```shell
   TARGETS="[<first target>,<second target>]"
   VALUES="[0,0]"
   PAYLOADS="[$(cast calldata '<first signature>' <arguments>),$(cast calldata '<second signature>' <arguments>)]"
   ZERO=0x0000000000000000000000000000000000000000000000000000000000000000
   SALT=$(cast keccak "<label>")
   DELAY=$(cast call $TIMELOCK 'getMinDelay()(uint256)' --rpc-url $RPC)
   ```

   `VALUES` holds one `0` for each call. The runbook gives each call's target, such as `$VAULT` or
   `$REGISTRY`, and its signature and arguments.

2. Have the proposer schedule the batch.

   **Changes the live system.** The owner must approve the batch, and the multisig must sign it.

   ```shell
   cast send $TIMELOCK 'scheduleBatch(address[],uint256[],bytes[],bytes32,bytes32,uint256)' "$TARGETS" "$VALUES" "$PAYLOADS" $ZERO $SALT $DELAY --rpc-url $RPC --account <proposer>
   ```

3. After the delay, check that the batch is ready:

   ```shell
   OP=$(cast call $TIMELOCK 'hashOperationBatch(address[],uint256[],bytes[],bytes32,bytes32)(bytes32)' "$TARGETS" "$VALUES" "$PAYLOADS" $ZERO $SALT --rpc-url $RPC)
   cast call $TIMELOCK 'isOperationReady(bytes32)(bool)' $OP --rpc-url $RPC
   ```

   The second command prints `true`.

4. Have the executor run the batch once the runbook's condition for it holds.

   **Changes the live system.** The owner must approve the execution.

   ```shell
   cast send $TIMELOCK 'executeBatch(address[],uint256[],bytes[],bytes32,bytes32)' "$TARGETS" "$VALUES" "$PAYLOADS" $ZERO $SALT --rpc-url $RPC --account <executor>
   ```

   If a call reverts, `cast` prints its error, and the runbook lists the errors to expect.

5. Confirm that the batch has run, as step 5 of
   [Calling a contract through the timelock](#calling-a-contract-through-the-timelock) does with
   this `OP`, and read each call's effect with the reads that the runbook names.

A batch that reverts, or that a runbook replaces with another, stays ready, and anyone with the
executor role can run it later. Have the proposer cancel a batch that will not run, with its `OP`
from step 3, before scheduling its replacement.

**Changes the live system.** The owner must approve the cancellation.

```shell
cast send $TIMELOCK 'cancel(bytes32)' $OP --rpc-url $RPC --account <proposer>
```

## Chain finality

Production custody runs on Arbitrum One, whose chain id is 42161. The trading server uses two block
tags, which `crates/node/src/config/custody.rs` defines:

- **`latest`, less `confirmations` blocks,** decides when a deposit is credited to a trading
  account.
- **`finalized`** decides when a deposit's principal can be withdrawn. Every checkpoint must also be
  `finalized` before the next one is relayed.

On Arbitrum One, a block becomes `finalized` once its batch has been posted to Ethereum and about
one Ethereum finality period, around 15 minutes, has passed. The wait is therefore the time until
the block's batch is posted, plus about 15 minutes.

A withdrawal is authorised only once a checkpoint that covers it is `finalized`. A simulation found
that wait to be 13 to 27 minutes, assuming Ethereum's 13-minute finality, `checkpoint_every = 1000`,
and 30 to 100 instructions a second. The wait has not been measured on Arbitrum One, and the time
until a block's batch is posted comes on top. Measure the wait on the launch deployment, and replace
any figure here that the measurement contradicts.

A governance change such as `setSigner` or `setCurrentRelease` also reaches the trading server only
once its block is `finalized`.

## Deadlines that run while the trading server is down

These deadlines count from the vault's last accepted checkpoint, `lastCheckpointAt()`. The table
gives each deadline's launch value. The contract's own value is the one that applies, so read it.

| After | What happens | Read it with |
| --- | --- | --- |
| `staleAfter`, 1 day at launch | The vault refuses every deposit, including house capital, until a checkpoint is posted. | `cast call $VAULT 'staleAfter()(uint64)' --rpc-url $RPC` |
| The liveness timeout, 3 days | While rotation is on, anyone may promote the top standby, as [Running as a standby](standby.md) describes. | `cast call $REGISTRY 'parameters()(uint64,uint64,uint16,uint16,uint16,uint256,uint64)' --rpc-url $RPC`, whose last value is the timeout |
| The timelock's delay, 3 days | Any governance fix, including `setSigner`, takes at least this long to run. | `cast call $TIMELOCK 'getMinDelay()(uint256)' --rpc-url $RPC` |
| `exitPeriod`, 14 days | Anyone may start the permanent exit against the last posted root. Signed withdrawals, checkpoints and distribution claims then stop for good. | `cast call $VAULT 'exitPeriod()(uint64)' --rpc-url $RPC` |

**A recovery that needs the timelock must start early enough for the delay to run out before
`exitPeriod`.** [Checkpoints cannot resume](incident-response.md#checkpoints-cannot-resume) says
when to decide.

## Download the public log and verify it

The log is public. Its completed segments and their manifest are served under `/v1/archive/`, and
anyone can replay them.

1. Download the manifest and every segment it lists:

   ```shell
   mkdir -p archive
   curl -f --retry 5 -o archive/manifest.jsonl $API/v1/archive/manifest.jsonl
   jq -r '.file' archive/manifest.jsonl | xargs -I{} curl -f --retry 5 -o archive/{} $API/v1/archive/{}
   ```

2. Replay the log:

   ```shell
   $ENGINE replay --log archive
   ```

   The command prints a line that begins `verified:` and names the number of instructions and
   checkpoints, the final root, the protocol versions, and the operator. A second line names the
   checkpoint signers. Any other ending is a failure.

`replay` verifies every checkpoint's root and signature. To pin the signer as well, add
`--signer $SIGNER`. For a log whose signer has changed, give one `--attestation` and `--release`
pair for each signer, in order.

The archive holds completed segments only. The segment still being written, which holds the newest
instructions, is not in it.

## Where things live on the host

The image mounts one data disk at `/data`, and nothing else persists.

| Path | What it holds | Keep a copy? |
| --- | --- | --- |
| `/data/config.toml` | The configuration. Its SHA-256 hash is bound into the attestation. | Yes. It is also the release's configuration. |
| `/data/log` | The event log and `manifest.jsonl`, with `checkpoint-submission.json`, `log-head-submission.json` and their `.lock` files beside the segments. | Yes, continuously, as described below. |
| `/data/history` | The account history index and the marks cache, both rebuilt from the log. | No. |
| `/data/snapshots` | With `[snapshots]`, the recovery caches, the authenticated account history index and the segment catalog. | No. |
| `/data/daily-snapshots` | With `[daily_snapshots]`, the engine's state at the first checkpoint of each UTC day, and its manifest. | Optional. [Daily snapshots](restart-and-recovery.md#daily-snapshots) describes them. |
| `/data/alerts` | The alert outbox, which holds pages not yet delivered. | Yes, together with the log. |
| `/data/keys` | The sealed enclave keys, one file for each release and generation, and the delivered secrets, one `<release id>.secrets.json` for each release. | A copy is useless on another chip. Keep one anyway, as proof of what was sealed. |
| `/data/custody-cursor.json`, `/data/staking-cursor.json` | The points from which the deposit and staking scans resume. | Optional. Without a cursor, the scan starts at `custody.evm.start_block`. |
| `/data/new-key`, `/data/predecessor` | One-off markers, which the image removes before it starts the engine. | Never. |

Copy the log off the host by downloading the archive, as
[Download the public log and verify it](#download-the-public-log-and-verify-it) describes, to two
stores that do not share a failure. Keep the active segment from a standby's copy, as
[Running as a standby](standby.md) describes. A standby and a successor operator read from the
archive mirrors when the trading server is down.
