# Reference

The variables, tools and checks the other runbooks use. Read this once, and keep the shell you set
the variables in open.

## Tools on the admin workstation

- `engine`, the node's binary, built from the release tag with the production feature, so its
  config checks match the node's: `cargo build --release -p engine-node --features production`.
  It provides `admin`, `replay`, `verify-attestation`, `verify-response`, `verify-frames`,
  `exit-proof` and `ct-watch`. `engine <command> --help` lists the flags.
- `cast` from Foundry, to read the contracts and to send the transactions you are approved to send.
- `curl`, `jq`, `xxd` and `openssl`.

Never put a private key on a command line or in shell history. Use a hardware wallet, a Foundry
keystore (`--account <name>`), or, for the operator's admin key, a file readable by you alone
(`--key-file`).

## Variables

```sh
export API=https://api.example.com     # the node's public API: api.tls.acme.domain
export ENGINE=./engine                  # the binary above
export CONFIG=node.toml                 # the config the running release was started with
export VAULT=0x...                      # custody.vault
export REGISTRY=0x...                   # custody.evm.staking.registry.contract
export USDC=0x...                       # custody.evm.usdc
export TIMELOCK=0x...                   # the vault's governance(): cast call $VAULT 'governance()(address)'
export RPC=https://...                  # one custody provider, for these reads
```

Check: `cast chain-id --rpc-url $RPC` prints `42161`, and `cast call $VAULT 'usdc()(address)' --rpc-url $RPC`
prints the USDC address in `custody.evm.usdc`.

## The node's attested key and its pin

Every `engine admin` call is pinned to the node's TLS key, so the call fails if anything but the
attested enclave answers. Take the pin from an attestation you have verified.

```sh
curl -sf $API/v1/attestation > attestation.json
$ENGINE verify-attestation --attestation attestation.json --release release.toml --processor <Genoa|Milan>
BUNDLE=$(jq -r '.bundle[2:]' attestation.json)
export PIN=${BUNDLE: -64}                 # bundle bytes 73..105: the SHA-256 of the TLS key
export SIGNER=$(jq -r .enclave_address attestation.json)
```

Check: `verify-attestation` exits 0. `release.toml` is the manifest published with the release the node
runs (see [deploy-and-rollback.md](deploy-and-rollback.md)); `--processor` is the codename of the
host's CPU generation. `$PIN` equals `jq -r '.tls_public_key_sha256[2:]' attestation.json`.
With `[tee.sealing]` the TLS key survives restarts and upgrades, so the pin changes only when new
keys are made ([key-recovery.md](key-recovery.md)).

An admin call, once `OPERATOR_KEY_FILE` names a file holding the operator's key:

```sh
$ENGINE admin --config $CONFIG --node $API --pin $PIN <command>
```

`--pin` goes before the command. The config must be the one the node runs, because it names the
operator key and the chain and vault the command is signed for.

## Health in one minute

```sh
curl -sf $API/v1/health                                   # prints ok
curl -sf $API/v1/operator | jq .                          # operator in effect, pending rotation, last election
curl -sf $API/v1/markets | jq -c '.[] | {id, symbol, open, reduce_only, mark, stress_level}'
curl -s http://<metrics host>:9464/metrics | grep -E '^engine_(withdrawals_paused|last_checkpoint_timestamp_seconds|alert_pages_pending|sequencer_queue_depth|tls_certificate_pending) '
```

- `engine_withdrawals_paused 0` means withdrawal authorizations are being signed. `1` is a pause;
  [withdrawal-stalls.md](withdrawal-stalls.md) says which.
- `engine_last_checkpoint_timestamp_seconds` is the sequenced time of the command the last signed
  checkpoint covers. It trails the clock while a checkpoint is held back for the previous one to
  become final.
- `engine_alert_pages_pending` above 0 for long means pages are not being delivered.
- Every market with an `open` session and a feed should show a `mark`. A market with no `mark`
  and `open: true` is the `market_no_price` case ([feed-and-custody-failures.md](feed-and-custody-failures.md)).

The metrics listener is plain HTTP and carries no per-account data. Reach it only from the
monitoring network.

## Read the vault and the registry

```sh
cast call $VAULT 'latestSeq()(uint64)'            --rpc-url $RPC   # seq of the last posted checkpoint
cast call $VAULT 'latestRoot()(bytes32)'          --rpc-url $RPC
cast call $VAULT 'lastCheckpointAt()(uint64)'     --rpc-url $RPC   # block time of the last checkpoint
cast call $VAULT 'checkpointSigner()(address)'    --rpc-url $RPC   # 0x0 means the guardian retired it
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

These read the latest block. A node acts on the `finalized` block, so a change you have just sent
can take the time below to take effect on the node.

## Calling a contract through the timelock

Every governance call in these runbooks (`setCurrentRelease`, `setSigner`, `setReleaseApproval`,
`setRotation`, `setPauses`) runs through the vault's timelock: the proposer schedules it, the delay
passes, and the executor runs it. This is the pattern; each runbook gives the target and the data.

```sh
TARGET=$VAULT                                 # or $REGISTRY
DATA=$(cast calldata '<function signature>' <arguments>)
ZERO=0x0000000000000000000000000000000000000000000000000000000000000000
SALT=$(cast keccak "<a label no earlier call used>")
DELAY=$(cast call $TIMELOCK 'getMinDelay()(uint256)' --rpc-url $RPC)

# 1. The proposer schedules it. With a multisig as proposer, submit this call as a multisig transaction.
cast send $TIMELOCK 'schedule(address,uint256,bytes,bytes32,bytes32,uint256)' $TARGET 0 $DATA $ZERO $SALT $DELAY --rpc-url $RPC --account <proposer>

# 2. After the delay, check it is ready, then the executor runs it.
OP=$(cast call $TIMELOCK 'hashOperation(address,uint256,bytes,bytes32,bytes32)(bytes32)' $TARGET 0 $DATA $ZERO $SALT --rpc-url $RPC)
cast call $TIMELOCK 'isOperationReady(bytes32)(bool)' $OP --rpc-url $RPC
cast send $TIMELOCK 'execute(address,uint256,bytes,bytes32,bytes32)' $TARGET 0 $DATA $ZERO $SALT --rpc-url $RPC --account <executor>
```

Check: `isOperationDone(bytes32)(bool)` on `$OP` prints `true` after the execute. Then read the
effect from the contract (each runbook names the call), and wait for the block that holds it to be
`finalized` before the node acts on it. If the execute reverts, `cast` prints the contract's error
name; the runbook for the call lists the ones to expect. To withdraw a scheduled call before it
runs, the proposer sends `cast send $TIMELOCK 'cancel(bytes32)' $OP --rpc-url $RPC --account <proposer>`.

## Chain finality

Production custody is Arbitrum One (chain id 42161). The node uses two block tags
(`crates/node/src/config/custody.rs`):

- **`latest`, less `confirmations` blocks, credits a deposit** to a trading account.
- **`finalized` releases the principal for withdrawal**, and every checkpoint must be `finalized`
  before the next is relayed. On Arbitrum One `finalized` follows the posting of a block's batch to
  Ethereum by about one Ethereum finality period, around 15 minutes. So a block is `finalized` after
  the gap until its batch is posted, plus about 15 minutes.

A withdrawal is authorised only once a checkpoint that covers it is `finalized`. The wait was
simulated at 13 to 27 minutes, assuming Ethereum's 13-minute finality, `checkpoint_every = 1000` and
30 to 100 commands a second (#469). It was not measured on Arbitrum One, and the batch-posting gap
comes on top. Measure it on the launch deployment and replace any figure here that it contradicts. A governance change such as `setSigner` or
`setCurrentRelease` also reaches the node only once its block is `finalized`.

## Deadlines that run while the node is down

Counted from the last accepted checkpoint (`lastCheckpointAt()`). The decided values are in the
issue noted; the contract's own value is authoritative, so read it.

| After | What happens | Read it with |
| --- | --- | --- |
| `staleAfter` (set at deployment, #436) | The vault refuses every deposit, house capital included, until a checkpoint posts. | `cast call $VAULT 'staleAfter()(uint64)'` |
| liveness timeout (3 days, #360) | With rotation on, anyone may promote the top standby ([standby.md](standby.md)). | `cast call $REGISTRY 'parameters()(uint64,uint64,uint64,uint16,uint16,uint16,uint16,uint256,uint64)'`, last value |
| timelock delay (3 days, #360) | Any governance fix, `setSigner` included, takes at least this long to run. | `cast call $TIMELOCK 'getMinDelay()(uint256)'` |
| `exitPeriod` (14 days, #360) | Anyone may start the permanent exit against the last posted root. Signed withdrawals, checkpoints and distribution claims then stop for good. | `cast call $VAULT 'exitPeriod()(uint64)'` |

A recovery that needs the timelock must start early enough for the delay to run out before
`exitPeriod`. The incident runbook says when to decide.

## Download the public log and verify it

The log is public: completed segments and a manifest, served at `/v1/archive/`. Anyone can replay it.

```sh
mkdir -p archive
curl -f --retry 5 -o archive/manifest.jsonl $API/v1/archive/manifest.jsonl
jq -r '.file' archive/manifest.jsonl | xargs -I{} curl -f --retry 5 -o archive/{} $API/v1/archive/{}
$ENGINE replay --log archive
```

Check: the last line of `replay` begins `verified:` and names the commands, checkpoints, final
root, protocol version and operator, and a second line names the checkpoint signers. Any other
ending is a failure. `replay` verifies every checkpoint's root and signature. To also pin the
signer, add `--signer $SIGNER`; for a log whose signer has changed, give one `--attestation`,
`--release` pair per signer.

The archive holds completed segments only. The segment still being written, and so the newest
commands, are not in it.

## Where things live on the host

The image mounts one data disk at `/data`. Nothing else persists.

| Path | Holds | Keep a copy? |
| --- | --- | --- |
| `/data/config.toml` | The config. Its SHA-256 is bound into the attestation. | Yes. It is also the release's config. |
| `/data/log` | The event log and `manifest.jsonl`; `checkpoint-submission.json` and its `.lock` beside the segments. | Yes, continuously. See below. |
| `/data/history` | The account history index and marks cache, both rebuilt from the log. | No. |
| `/data/alerts` | The alert outbox: pages not yet delivered. | Yes, with the log. |
| `/data/keys` | Sealed enclave keys, one file per release and generation. | A copy is useless on another chip, but keep it: it is the proof of what was sealed. |
| `/data/custody-cursor.json`, `/data/staking-cursor.json` | Where the deposit and staking scans resume. | Optional. A lost cursor makes the scan start at `custody.evm.start_block`. |
| `/data/new-key`, `/data/predecessor` | One-off markers the image removes before it starts the engine. | Never. |

Copy the log off the host by downloading the archive (above) to two stores that do not share a
failure, and keep the active segment from a standby's copy ([standby.md](standby.md)). The archive
mirrors are what a standby and a successor operator read from when the node is dead.
