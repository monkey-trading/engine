# Standing as an operator candidate

Use this runbook when you stand for election as the operator, take daily turns as a member of the
rotation set, or hold the operator's role and must hand it on or upgrade it. It also covers the
alerts that a member's trading server raises around a turn, parameter votes and operator faults.
Running a standby is covered by [Running as a standby](standby.md). Read [Reference](reference.md)
first, because this page uses its variables and its pattern for timelock calls. Commands and their
output call a running trading server a node.

- [How the operator is chosen](#how-the-operator-is-chosen)
- [Variables](#variables)
- [What a candidate needs](#what-a-candidate-needs)
- [The candidate's configuration](#the-candidates-configuration)
- [Stand for election](#stand-for-election)
- [Withdraw a candidacy](#withdraw-a-candidacy)
- [Register the incumbent before rotation is turned on](#register-the-incumbent-before-rotation-is-turned-on)
- [Take over after an election](#take-over-after-an-election)
- [Handing over to the next operator](#handing-over-to-the-next-operator)
- [Upgrade while you are the operator](#upgrade-while-you-are-the-operator)
- [Daily turns](#daily-turns)
- [Minute-scale liveness](#minute-scale-liveness)
- [Parameter votes](#parameter-votes)
- [Operator faults](#operator-faults)

## How the operator is chosen

The operator runs the enclave's host, holds the admin key, and is chosen on chain. Stakers delegate
amounts of their MONKEY to candidates. Anyone may close an election epoch, and closing it elects
the eligible candidate with the most delegated stake.

**Rotation is off at launch.** Until the timelock turns rotation on, the deployer is the only
operator, and none of the election steps on this page has any effect.

The launch registry names a schedule, `OperatorSchedule`. Once rotation is on, the registry
therefore rotates the operator in daily turns instead of electing one, as
[Daily turns](#daily-turns) describes. Every turn still uses the registration, handover and upgrade
procedures that the sections before it describe.

The registry's launch parameters are these:

| Parameter | Launch value |
| --- | --- |
| Election epoch | 14 days |
| Finality margin, added to the seven-day notice | 1 day |
| Eligibility | 1% of total stake |
| Quorum | 25% of total stake |
| Candidate cap | 25 candidates |
| Minimum self-bond | 0.1% of MONKEY's maximum supply |
| Liveness timeout | 3 days |

The deployed values are the ones that count. To read them, run:

```shell
cast call $REGISTRY 'parameters()(uint64,uint64,uint16,uint16,uint16,uint256,uint64)' --rpc-url $RPC
```

The command prints, in order, the epoch length and the finality margin in seconds, the eligibility
share and the quorum in basis points, the candidate cap, the minimum self-bond in MONKEY base units,
and the liveness timeout in seconds. Every time the registry uses is in seconds.

## Variables

The procedures on this page use these variables, as well as those that
[Variables](reference.md#variables) sets up:

```shell
export STAKING=$(cast call $REGISTRY 'staking()(address)' --rpc-url $RPC)
export SCHEDULE=$(cast call $REGISTRY 'schedule()(address)' --rpc-url $RPC)
export MONKEY=$(cast call $VAULT 'monkey()(address)' --rpc-url $RPC)
export CANDIDATE=<candidate address>
export VOTE=<vote contract>
```

`<candidate address>` is the address of the candidate account. `<vote contract>` is the address in
`custody.evm.staking.registry.vote.contract`. `$SCHEDULE` is the zero address while the registry
names no schedule. In the commands below, `<candidate>` is the Foundry keystore that holds the
candidate account's key.

## What a candidate needs

| Need | Detail |
| --- | --- |
| A bare-metal AMD SEV-SNP host | The candidate's own host, started with the image's launch flags, as [Deploy and rollback](deploy-and-rollback.md) describes, and with a data disk. |
| A hostname and a CAA record | The candidate's `apiHostname` needs DNS, port 443 forwarded unmodified to the trading server, and a CAA record that names the chosen certificate authority and TLS-ALPN-01, as [Before the first certificate](certificate-renewal-failure.md#before-the-first-certificate) describes. The enclave orders its certificate for this name under the ACME account that moves with the keys. |
| Its own custody providers | At least three providers, run by different operators, that serve archive state back to the vault's deployment block. The quorum that credits deposits is the candidate's own choice, which is why the timelock approves each release. |
| Its market-data accounts and secrets | The venues and vendors in its configuration, with their API keys. The launch operators share one account for each of Massive, TraderMade and Alpaca, so a launch candidate takes those keys from the other operators. No secret moves from the incumbent through the node. The candidate delivers the secrets to its own enclave, as [Secrets](key-recovery.md#secrets) describes. |
| A checkpoint endpoint and a funded relayer | The candidate's own, as [Checkpoint endpoint](feed-and-custody-failures.md#checkpoint-endpoint) describes. |
| A seed relayer | Ethereum providers, a beacon node, and a funded relayer key that is separate from the checkpoint relayer's, as [Seed relay](seed-relay.md) describes. |
| An alert receiver | `[alerts.critical]` is required with real attestation. |
| An admin key | Made and kept offline, ideally on a hardware wallet. The admin key signs `engine admin` commands and the candidate's registration. |
| A candidate account | An Arbitrum account that holds the self-bond in staked MONKEY, and ETH for gas. It may differ from the admin key. While its candidate is the operator in effect, the candidate account can also move the vault's release to any release that the timelock approved for it, and the operator in effect cannot deregister to revoke that power. Keep the candidate account like the admin key: offline, on a hardware wallet or behind a multisig. |
| A Certificate Transparency watcher | Run on a host that is independent of the trading server's, and started before the first certificate is issued. |
| An archive mirror | At least one store that does not share a failure with the trading server. A successor reads the log from the mirrors when the trading server is dead. |

## The candidate's configuration

The candidate's trading server must start on the log that the incumbent wrote. Everything that the
log records must therefore match the running log, and everything else is the candidate's own. Start
from [production-node.toml](production-node.toml).

| Must equal the running log | The candidate's own |
| --- | --- |
| `[genesis]`: the launch genesis, including the close-spread ceilings and staking | `node.listen`, the `[api]` limits and origins, and `[api.tls.acme]` with its hostname and contact |
| `node.auth = "eip712"`, `custody.chain_id` and `custody.vault` | The `[custody.evm]` providers and their credentials, and the `[checkpoints]` endpoint and relayer |
| The `[custody.evm.staking]` contract and registry addresses | The `[feeds]` venues, vendors and symbols, and the names of their secrets |
| The `[access]` `source` and `max_age`, which the log records once | The `[time]` servers, `[metrics]`, `[alerts]`, and the `[access]` endpoint |
| | `operator.public_key`, which is **the candidate's own admin key**, the `[tee.sealing]` directory, and the handover listener |

The log records the incumbent's key until the election takes effect. A configuration that names the
admin key of a pending election, on the same chain and vault, is accepted.

A start whose configuration differs from what the log records is refused with `log_recovery_failed`.
The timelock approves the configuration's hash, so every change after the approval needs a new
approval.

## Stand for election

Use this procedure to register a candidacy and have its release approved, so that the candidate can
be elected or adopted into a rotation set.

1. Build the candidate's release, meaning its image and configuration, and publish it, as
   [Prepare a release](deploy-and-rollback.md#prepare-a-release) describes. Keep the two values that
   the build gives you: `$DIGEST`, the 48-byte launch measurement, and `$CONFIG_HASH`, the
   configuration's SHA-256.

2. Stake the self-bond from the candidate account.

   **Changes the live system.** The owner must approve these transactions. They spend the
   candidate's own funds, and the owner decides whether this party may stand.

   ```shell
   cast send $MONKEY 'approve(address,uint256)' $STAKING $AMOUNT \
     --rpc-url $RPC --account <candidate>
   cast send $STAKING 'stake(uint256)' $AMOUNT --rpc-url $RPC --account <candidate>
   ```

   `$AMOUNT` is the MONKEY to stake, in base units. New stake is undelegated until a delegation
   gives it a candidate.

3. Delegate the stake.

   **Changes the live system.** The owner must approve this transaction.

   ```shell
   cast send $STAKING 'delegate(address,uint256)' $CANDIDATE $AMOUNT \
     --rpc-url $RPC --account <candidate>
   ```

   Delegating to the candidate itself is optional, and any amount up to the undelegated stake may be
   delegated. Votes may come from any staker.

4. Confirm the stake and the delegations:

   ```shell
   cast call $STAKING 'stakes(address)(uint256)' $CANDIDATE --rpc-url $RPC
   cast call $STAKING 'delegationsOf(address)((address,uint256)[])' $CANDIDATE --rpc-url $RPC
   ```

   The first command prints a stake of at least the bond, and the bond must be at or above the
   minimum self-bond. The second command lists the delegations that the candidate account made.

5. Have the admin key authorise the candidate. The admin key signs an EIP-712 authorisation of this
   candidate, so that no one can register a key that the candidate did not choose. The registry
   computes the digest, and the admin key signs the digest as a hash, without the Ethereum message
   prefix.

   ```shell
   DIGEST_REG=$(cast call $REGISTRY 'registrationDigest(address,address)(bytes32)' \
     $CANDIDATE <admin key address> --rpc-url $RPC)
   SIG=$(cast wallet sign --no-hash $DIGEST_REG --ledger)
   ```

   `<admin key address>` is the admin key's address. For a keystore instead of a Ledger, replace
   `--ledger` with `--account <keystore>`. `$SIG` then holds the signature.

6. Register the candidacy from the candidate account.

   **Changes the live system.** The owner must approve this transaction. It locks the bond.

   ```shell
   cast send $REGISTRY 'register((address,address,uint256,bytes,bytes32,string,string),bytes)' \
     "(<admin key address>,<enclave>,$BOND,$DIGEST,$CONFIG_HASH,<api hostname>,<handover endpoint>)" \
     $SIG \
     --rpc-url $RPC --account <candidate>
   ```

   - `<enclave>` is the address that a standby of this release would register. It matters only to a
     takeover after a liveness promotion, and `setEnclave` replaces it then, as
     [Running as a standby](standby.md) describes. Use the address of a rehearsal boot of this
     release, or any address you control.
   - `$BOND` is the self-bond, in MONKEY base units.
   - `<api hostname>` is the candidate's API hostname, such as `api.candidate.example`.
   - `<handover endpoint>` is the `host:port` at which a successor reaches the trading server's
     `tee.sealing.handover.listen`, such as `candidate.example:9471`.

   The transaction reverts with one of these errors:

   - `AlreadyRegistered`, when the candidate account already has a candidacy;
   - `BondBelowMinimum`, when the bond is below the minimum self-bond;
   - `AdminKeyTaken`, when another candidate registered the admin key;
   - `UnauthorizedAdminKey`, when the signature is not the admin key's over this candidate;
   - `InvalidEndpoint`, when the hostname or the handover endpoint is not acceptable.

7. Confirm the registration:

   ```shell
   cast call $REGISTRY 'candidateOf(address)((address,address,uint256,bytes32,uint64,string,string))' \
     $CANDIDATE --rpc-url $RPC
   cast keccak $(cast concat-hex $DIGEST $CONFIG_HASH)
   ```

   The first command prints the registration. Its fourth field, the release, equals the hash that
   the second command prints.

8. Have the timelock approve the release for the candidacy, with the pattern in
   [Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock)
   and this target and data.

   **Changes the live system.** The owner must approve this contract call. The owner reviews the
   published configuration first, including its providers, vendors and hostname, because this
   approval is what makes the candidate eligible.

   ```shell
   TARGET=$REGISTRY
   DATA=$(cast calldata 'setReleaseApproval(address,bytes,bytes32,bool)' \
     $CANDIDATE $DIGEST $CONFIG_HASH true)
   ```

9. Confirm that the candidate is eligible:

   ```shell
   cast call $REGISTRY 'isEligible(address)(bool)' $CANDIDATE --rpc-url $RPC
   cast call $STAKING 'votes(address)(uint256)' $CANDIDATE --rpc-url $RPC
   ```

   The first command prints `true` once the release is approved and the candidate's votes, which the
   second command prints, reach the eligibility share of total stake.

10. Run a standby, as [Running as a standby](standby.md) describes, so that the candidate is ready
    for a promotion. A candidate that wins an election takes the keys by handover instead, as
    [Take over after an election](#take-over-after-an-election) describes.

**How to tell it is resolved.** Step 9 prints `true`, and the registration names the release that
the timelock approved.

Withdrawing an approval delists the candidate until it moves to another release that is still
approved for it. To keep a candidate off the list, withdraw every release approved for it. A
candidate changes its release without leaving, with steps 2 and 3 of
[Upgrade while you are the operator](#upgrade-while-you-are-the-operator). An election names the
release that the candidacy holds when the epoch closes.

## Withdraw a candidacy

Use this procedure to remove a candidacy and release its bond.

A candidate cannot leave in any of these cases:

- while it is the operator in effect and no turn names the operator;
- while it is elected and waiting to take effect;
- while a turn names the operator or a drawn turn runs, and the registry's `holdsTurn` names the
  candidate.

`holdsTurn` names a candidate in any of these cases:

- It has held any assignment of the turn in effect, or of the last drawn turn.
- Its release signed the newest log head.
- It may hold the keys that the member of the turn in effect takes. A member of the turn drawn
  before the turn in effect may hold them. Once a head latched that earlier turn, or a promotion
  named its member, only that turn's last member counts. While no member of a turn has taken the
  keys, the members of the turns before it count as well, up to seven turns back.
- It is the source of the earliest turn reached in that way, when that turn is the first turn, the
  first turn after an installation, or the turn seven turns back.

A holder is free from the boundary of the turn after the one whose member took its keys. The
exception is the source of the turn seven turns back, which stays held until a later turn starts or
an installation ends the turns.

1. Deregister from the candidate account.

   **Changes the live system.** The owner must approve this transaction.

   ```shell
   cast send $REGISTRY 'deregister()' --rpc-url $RPC --account <candidate>
   ```

   The transaction reverts with `OperatorCannotLeave` in the cases above, and with `NotRegistered`
   for an account that has no candidacy.

**How to tell it is resolved.** The transaction succeeds, and the bond is released one staking
cooldown later.

## Register the incumbent before rotation is turned on

Use this procedure before the owner turns rotation on for the first time, and check its release
condition before every later `setRotation(true)`.

An election hands the keys to the winner from the incumbent's registered handover endpoint. The
registry records the handover endpoint and the API hostname of the operator in effect when an
election is made. The initial operator, which the deployment sets, is not registered until it
registers itself. If it has not registered, the successor finds no endpoint, because
`handoverSource()` is empty, and refuses to start.

**The contracts accept an election that leaves `handoverSource()` empty, and the successor then
cannot start. Do not work around this on a live system. The owner decides how to proceed.**

Registering does not change the release that the registry names for the operator in effect. Until
that release moves, it is the release that the deployment named, and the vault names it once
rotation is on. If the two releases differ, turning rotation on makes the vault name another
release, and the running trading server's keys no longer open on a restart, which raises
`release_superseded`.

1. Register the initial operator as a candidate, with steps 2 to 9 of
   [Stand for election](#stand-for-election). Use the admin key's own address as the candidate
   account, because the initial operator's candidate identity is its admin key. Register its real
   hostname and handover endpoint, and the release that the vault names now, which the timelock
   approves.

   **Changes the live system.** Each step names the approval it needs.

2. Move the candidacy to that release, with step 3 of
   [Upgrade while you are the operator](#upgrade-while-you-are-the-operator). A move to the release
   that the candidacy already holds keeps its approval.

   **Changes the live system.** The owner must approve this transaction.

3. Confirm that the registry and the vault name the same release:

   ```shell
   cast call $REGISTRY 'operatorInEffect()((address,address,bytes32,uint64))' --rpc-url $RPC
   cast call $VAULT 'currentRelease()(bytes32)' --rpc-url $RPC
   ```

   The third field of the first command's output equals the second command's output.

4. Once an election exists, confirm that the registry records the incumbent's endpoint:

   ```shell
   cast call $REGISTRY 'handoverSource()((address,bytes32,string,string))' --rpc-url $RPC
   ```

   The output names the incumbent, its release, its hostname and its handover endpoint.

**How to tell it is resolved.** Step 3 shows the same release twice. This is a precondition of every
`setRotation(true)`, not only the first: `operatorInEffect().release` must equal `currentRelease()`.

An upgrade made while rotation is off moves only the vault's release, so it must also move the
registry's. The candidacy moves to the approved new release with steps 2 and 3 of
[Upgrade while you are the operator](#upgrade-while-you-are-the-operator). An installed operator
that is not registered is moved instead by calling `installOperator` again with the new release.

## Take over after an election

Use this procedure when an election names your candidate as the next operator. The contracts fix the
order of events.

1. Close the epoch once it has ended, if no one else has. Anyone may send this transaction.

   **Changes the live system.** The owner must approve sending it.

   ```shell
   cast send $REGISTRY 'closeEpoch()' --rpc-url $RPC --account <any>
   ```

   `<any>` is any funded keystore. Closing the epoch elects a candidate only when all of these
   hold:

   - rotation is on;
   - the registry names no schedule;
   - no election is pending.

   It then elects the eligible candidate with the most votes, or, on a tie, the one that registered
   earlier. It does so only when that candidate is not the operator in effect, and when the listed
   candidates' votes reach the quorum. The operator in effect serves no minimum term, and it keeps
   the role on a tie only when it registered first.

   Before the epoch ends, the transaction reverts with `EpochNotEnded(<time>)`.

2. Read the pending election:

   ```shell
   cast call $REGISTRY 'pendingElection()((uint64,address,address,bytes32,uint64,bool))' \
     --rpc-url $RPC
   ```

   The output gives the epoch, the candidate, its admin key, its release, `effectiveAt`, and whether
   the election is a liveness promotion. The election takes effect after the seven-day notice plus
   the finality margin of one day. When the engine mirrors the election, the incumbent's trading
   server raises `operator_elected`, a Warning. The engine applies your admin key from
   `effectiveAt`, and the vault names your release from then.

3. Prepare your trading server's data disk before `effectiveAt`. The disk holds `config.toml`, which
   is the approved release's configuration, byte for byte. In it, `mirrors` under
   `[tee.sealing.handover]` lists your archive mirrors of the incumbent's log. The disk holds
   nothing else: no `new-key` file, no `predecessor` file, and no sealed keys for this release in
   the vault's generation of it. Keep your own secrets in a bundle file on your workstation, because
   nothing comes from the incumbent.

4. After `effectiveAt`, once that block is `finalized`, fold the release change into the vault's
   generation. Anyone may send this transaction. Your trading server waits up to a minute for it,
   and then refuses to start.

   **Changes the live system.** The owner must approve this transaction.

   ```shell
   cast send $VAULT 'syncRelease()' --rpc-url $RPC --account <any>
   ```

5. Confirm the fold:

   ```shell
   cast call $VAULT 'currentRelease()(bytes32)' --rpc-url $RPC
   cast call $VAULT 'releaseGeneration()(uint64)' --rpc-url $RPC
   ```

   The first command prints your release's id, and the generation that the second prints has risen.

6. Start your trading server.

   **Changes the live system.** The owner must approve deploying this release and taking the
   incumbent's keys.

   The trading server holds first, with `secret_missing` raised and only its delivery listener up.
   Deliver your secrets bundle under your admin key, as [Secrets](key-recovery.md#secrets)
   describes, and the trading server goes on. Its console then shows, in order:

   - that it has no sealed keys and that rotation is on;
   - the incumbent, resolved from the registry, in a line that starts `election handover: `;
   - the incumbent's published log, fetched from its hostname or from your mirrors;
   - the keys, taken from the incumbent's handover endpoint.

   The incumbent checks your quote against the vendor's certificate chain and the vault's named
   release, stops sequencing, and sends the keys and the tail of its log. Your trading server
   confirms, and the incumbent acknowledges and powers off.

7. Confirm the takeover:

   ```shell
   curl -sf $API/v1/markets
   curl -sf $API/v1/operator | jq .operator
   cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC
   ```

   The first command answers with the markets. From `effectiveAt`, the second prints your admin
   key's address. The `latestSeq()` that the third prints rises over time. The enclave address and
   the signer epoch are unchanged, and the incumbent's VM is off.

**When to escalate.** If the takeover fails, follow
[If the handover does not finish](deploy-and-rollback.md#if-the-handover-does-not-finish), whose
rules are the same, and tell the owner. **Never stop the incumbent while it reports
`handover_unconfirmed`.**

## Handing over to the next operator

Use this procedure when you are the incumbent and `operator_elected`, a Warning, fires because an
election names another operator. Nothing needs stopping. Your task is to stay up and reachable until
the successor has the keys.

1. Confirm that the successor can reach you. Your handover listener, `tee.sealing.handover.listen`,
   must be reachable from the internet at the `handoverEndpoint` you registered, and your API
   hostname must still resolve. Check that your archive is served:

   ```shell
   curl -sf $API/v1/archive/manifest.jsonl | head -1
   ```

   The command prints the first line of the archive's manifest. The successor fetches the published
   log from your hostname or from its mirrors, and the tail segment of the log from the handover.

2. Wait for `effectiveAt`. At `effectiveAt` the vault names the successor's release, and your
   trading server raises `release_superseded`, which is expected. The successor connects and your
   trading server hands over. Your trading server then follows the successor, as a member does
   between its turns, and opens none of its keys again. A restart of it does the same.

3. If the successor has not connected by `effectiveAt`, your trading server raises
   `handover_overdue`, a Critical alert. It still holds the keys and keeps sequencing. Contact the
   successor, and ask it to check the same three things from its side. **Do not stop your trading
   server, because it is the only one that holds the keys.**

4. After the handover, keep your copy of the log and the attestation of your enclave. The log's
   earlier checkpoints verify only against that attestation. Your admin key signs nothing from
   `effectiveAt`.

**How to tell it is resolved.** The successor confirms the handover and your trading server follows
it. When the election is cancelled, because the timelock turned rotation off or a later election
names you again, both `operator_elected` and `handover_overdue` resolve.

**When to escalate.** Tell the owner if `handover_overdue` stays open and the successor cannot
connect.

## Upgrade while you are the operator

Use this procedure to move the operator's trading server to a new release while rotation is on.

While rotation is on, the vault's `setCurrentRelease` reverts with `RotationActive`, because the
vault names the release of the operator in effect. You upgrade by moving your candidacy to a release
that the timelock has approved for it. The vault names the new release at once, and the running
release hands the keys and the log to the new one, as in an ordinary upgrade. The timelock's delay
still stands between any release and the keys, because only the timelock approves releases.

**A move is one-way.** It withdraws the approval of the release it leaves, so moving back needs a
new approval.

In these steps the running release is `A` and the new release is `B`.

1. Prepare `B`, as [Prepare a release](deploy-and-rollback.md#prepare-a-release) describes. Keep the
   hostname and the handover endpoint you registered. The registry has no call that changes them
   while you are the operator, so `B` serves them once `A` has powered off.

2. Have the timelock approve `B` for your candidacy, as step 8 of
   [Stand for election](#stand-for-election) does, with `B`'s digest and configuration hash.

   **Changes the live system.** The owner must approve this contract call.

3. Move your candidacy to `B`, from the candidate account.

   **Changes the live system.** The owner must approve this transaction. From this transaction on, a
   restart of `A` finds no keys, and `A`'s approval is withdrawn, which the registry reports as
   `ReleaseApprovalChanged(<candidate>, <A's release>, false)`.

   ```shell
   cast send $REGISTRY 'setRelease(bytes,bytes32)' $DIGEST $CONFIG_HASH \
     --rpc-url $RPC --account <candidate>
   ```

   The transaction reverts with one of these errors:

   - `NotRegistered`, when the account has no candidacy;
   - `ReleaseNotApproved(<release>)`, when the approval in step 2 has not executed;
   - `ElectionPending`, when an election or promotion that replaces you, or names you, waits to take
     effect. Upgrade after it takes effect, or let the successor run its own release;
   - `CandidateCapReached`, when your candidacy was delisted and the list is full. The operator in
     effect may take one place beyond the cap, and after an installation the operator that the
     installation replaced may hold that place. The timelock frees a place by withdrawing an
     approval or by raising the cap.

4. Confirm that the registry and the vault name `B`:

   ```shell
   cast call $REGISTRY 'operatorInEffect()((address,address,bytes32,uint64))' --rpc-url $RPC
   cast call $VAULT 'currentRelease()(bytes32)' --rpc-url $RPC
   cast keccak $(cast concat-hex $DIGEST $CONFIG_HASH)
   ```

   The third field of the first output and the second output both equal the third output.

5. Fold the change into the vault's generation. Anyone may send this transaction. `B` reads the fold
   at finality, waits up to a minute for it, and then refuses to start. `A` hands over no keys until
   the fold lands.

   **Changes the live system.** The owner must approve this transaction.

   ```shell
   cast send $VAULT 'syncRelease()' --rpc-url $RPC --account <any>
   ```

6. Once the block that holds the fold is final, confirm the fold at finality:

   ```shell
   cast call $VAULT 'syncedRelease()(bytes32)' --block finalized --rpc-url $RPC
   cast call $VAULT 'currentRelease()(bytes32)' --block finalized --rpc-url $RPC
   cast call $VAULT 'releaseGeneration()(uint64)' --block finalized --rpc-url $RPC
   ```

   The first two outputs are equal, and the generation has risen by one. **Do not start `B` on a
   reading of the latest block. `B` checks at finality, and refuses to start if the block that holds
   the fold is not yet final.** Once that block is `finalized`, `A` raises `release_superseded`, a
   Critical alert, and keeps sequencing. The page is expected.

7. Start `B` with a `predecessor` file, as steps 3 to 7 of
   [Upgrade to a new release](deploy-and-rollback.md#upgrade-to-a-new-release) describe.

   **Changes the live system.** The owner must approve deploying `B`.

   **Do not start `B` without the `predecessor` file. With no sealed keys and no flag, a trading
   server takes the keys from the operator that the last election replaced, which `handoverSource()`
   names, not from `A`.**

**How to tell it is resolved.** `B` holds the keys and sequences. The enclave address, the signer
epoch and `$PIN` carry over from `A`.

To roll back before `B` takes the keys, have the timelock approve `A` again, as step 2 does for `B`.
Then move the candidacy back to `A` with steps 3 to 6, and make `A` safe to restart again, as
[Roll back](deploy-and-rollback.md#roll-back) describes.

A candidate that is not the operator moves its release in the same way, with steps 2 and 3. The move
has no effect on the vault.

An operator that the timelock installed, as
[When no standby qualifies](standby.md#when-no-standby-qualifies) describes, upgrades in the same
way once it is a registered candidate. It registers first, as
[Stand for election](#stand-for-election) describes, because `setRelease` reverts with
`NotRegistered` for an operator with no candidacy.

- Until the installed operator's first move, the registry names the release that the installation
  named. That first move withdraws the approval of both that release and the candidacy's release.
- Releases that the timelock approved for the candidacy before the installation stay approved.
  Withdraw any that the operator should not move to.
- The installation changes no candidacy. The operator that it replaced keeps its release, its
  approvals and its listing, and that operator's moves no longer reach the vault.

## Daily turns

Use this section when you are a member of a rotation set, or when one of the alerts below fires on a
member's trading server.

| Alert | Where to go |
| --- | --- |
| `release_superseded` at a turn's boundary | [After your turn](#after-your-turn) |
| `handover_tip_not_on_log` or `handover_tail_lagged` | [If the take is refused](#if-the-take-is-refused) |
| `standby_follow_failed`, `operator_log_rollback`, `replica_diverged`, `copy_tail_unaccepted` or `member_follow_blocked` | [Between your turns](#between-your-turns) |
| `log_check_pending`, `log_check_unattributable` or `log_check_unavailable` | [Withdrawals and the boundary's log check](#withdrawals-and-the-boundarys-log-check) |
| `turn_refused` | [A turn the engine refused](#a-turn-the-engine-refused) |

With a schedule named, each election close made while rotation is on adopts every eligible candidate
as the rotation set, weighted by its delegated stake. The set takes effect seven days plus the
finality margin after the close. Each close's set waits out its own notice, however many sets are
waiting.

- **The quorum** counts only the votes of eligible candidates that hold a promotable standby key,
  which are the candidates that the set adopts by their votes.
- **A member between standby keys** at a close, as while it moves to a new release, keeps its place
  for that close and for any close within one epoch length after it. Register the new release's
  standby key within an epoch, or the next close that reaches the quorum leaves the member out. Each
  key registered starts that epoch afresh for the next move.
- **Without a quorum,** the latest set stands, less the members that are no longer eligible. Each
  member carried over is weighted by no more than its votes at the close.

Turns are a day long. Turn `n` starts at its boundary, 22:30 UTC on day `n` of the Unix epoch. A
production trading server refuses a schedule whose `turnLength()` is anything other than 86400
seconds, and raises `custody_chain_inconsistent`. Shorter turns exist only in development builds
with the `short-turns` feature, which a production build refuses to compile.

### Draw a turn

Use this procedure as soon as a turn's draw deadline passes, which is a day before its boundary.
Anyone may draw a turn, from the deadline until the boundary. Draw it early, so that the engine
mirrors it well before its boundary. **A turn that is not drawn by its boundary is never drawn, and
the member before it stays named.**

1. Confirm the turn length:

   ```shell
   cast call $SCHEDULE 'turnLength()(uint64)' --rpc-url $RPC
   ```

   The command prints `86400`.

2. Draw the turn.

   **Changes the live system.** The owner must approve this transaction.

   ```shell
   cast send $SCHEDULE 'draw(uint64)' <turn> --rpc-url $RPC --account <any>
   ```

   `<turn>` is the turn's number, `n`.

3. If the turn has no relayed seed, send the draw again from a later block. The seed receiver must
   first fix the Arbitrum block whose hash seeds the turn, and the members' trading servers do that
   within seconds of the deadline. Until then, a draw only moves that fix on, and draws no one. **A
   block left unrecorded for 256 blocks, about a minute, is never recorded, and the turn can then
   never be drawn.**

4. Read the drawn turn:

   ```shell
   cast call $SCHEDULE \
     'turnOf(uint64)((address,uint16,uint8,uint32,uint64,uint64,bytes32,address))' <turn> \
     --rpc-url $RPC
   ```

   The output names the turn's member, its position, the reason it was named, its rotation set, its
   generation, the turn before it, its seed, and its source. You take the keys from the registered
   handover endpoint of the member of the turn before yours.

**How to tell it is resolved.** Step 4 names a member for the turn.

### Prepare a member

Use this procedure before rotation is turned on, and before each turn that you hold.

1. Upgrade every candidate in the rotation set before the timelock turns rotation on, with
   [Upgrade while you are the operator](#upgrade-while-you-are-the-operator). Every member must run
   the release that hands over only the tail of the log. Each request for the keys names the newest
   protocol version that the successor's build reads, which an older incumbent does not read. A
   member on this release therefore takes no turn from an older incumbent, neither the tail nor the
   whole log.

   **Changes the live system.** Each move names the approval it needs.

2. Configure your trading server with all of these:

   - `operator.rotation_enabled = true`;
   - `[log_heads]`, without which the trading server refuses to start;
   - `[tee.sealing]`, with a handover listener;
   - `custody.evm.staking.registry.schedule`, naming the schedule.

   **Leave `[snapshots]` unset on every rotation-set member.** A member that took the tail with
   `[snapshots]` configured does not start from its replica. It recovers from disk, waits for
   checkpoint finality, and may replay the whole log inside the switch. A start from a snapshot past
   the verified head also cannot say who sequenced the range, and raises `log_check_unattributable`.

3. Start the trading server with no key flag, and keep it running between your turns.

   **Changes the live system.** The owner must approve the deployment.

**How to tell it is resolved.** The trading server follows the operator in effect, as
[Between your turns](#between-your-turns) describes.

### Between your turns

While a turn names another member, your trading server follows the operator in effect. That is the
candidate that the registry names, reached at `https://` and its registered API hostname. Every
frame is verified against the vault's signer at the latest block. Your trading server does all of
these:

- It keeps a copy of the log, with a live replica that one replay builds when it starts following.
- It resolves the operator again at each boundary, and after a failed stint: a second later, then
  twice as long each time, up to a stint.
- From a minute before its own turn's boundary, it holds its keyless venue connections, such as
  Coinbase's and Kraken's, open beside the incumbent's, with their quotes held back until it takes the
  turn, so it sends fresh quotes from its first instruction. A connection that carries a key opens
  only when the turn starts (`warm = false`, the default for such a connection), because a vendor
  allows one live connection per account and the launch operators share one account for each of
  Massive, TraderMade and Alpaca. The incumbent closes its connections before it hands over, so the
  successor's is accepted. The markets those vendors price have no price for a few seconds at each
  daily handover, until their first quotes arrive.
- It pins the latest posted log head as its replica passes that head.

These alerts page what it finds. Each pages once per episode, and each resolves once the step works
again.

| Alert | What it means | What to do |
| --- | --- | --- |
| `standby_follow_failed` | The trading server could not follow the operator's stream. It resolves the operator in effect again before its next stint, and pages only once five minutes have passed since the turn in effect began, the time that turn's member has to take the keys and come up. | Check that the operator in effect is reachable at its registered hostname. If the alert persists, follow [Run the standby](standby.md#run-the-standby). |
| `operator_log_rollback` | The operator's log no longer holds entries that your copy holds, or the latest posted log head names another history than your copy holds at that seq. | Keep your copy, and tell the owner. |
| `replica_diverged` | An entry that the operator in effect sealed does not replay on your replica. Your copy does not take it, and the trading server rebuilds the replica and retries each poll. | Check the operator's published log with `engine replay`. |
| `copy_tail_unaccepted` | Your copy held instructions after its last checkpoint that no accepted key sealed. The alert's `seq` names the first, and its `key` names the key that sealed it, or `none`. The trading server copied them to `<log_dir>.unaccepted-<seq>` and cut the copy back to its last checkpoint, and the operator's stream refills it. | Keep the set-aside files for the owner. If `key` is a key whose promotion a reorganisation removed, the tail is an orphaned promotion's, and the promoted trading server raises `promotion_reorged`. Otherwise, an archive mirror that served your copy, or your host, forged it. |
| `member_follow_blocked` | One step of following has failed two polls in a row. The alert's `step` names it. | Check that step's source, as the list below describes. |

The steps that `member_follow_blocked` names, and the source to check for each, are these:

- `read_turn` reads the turn at trusted time. Check the custody RPC providers, the vault, the
  schedule and the Roughtime servers.
- `resolve_operator` resolves the operator in effect. Check the registry's hostname for the
  operator.
- `open_copy` opens the copy and replays it into the replica, checking every seal. Check the copy's
  disk, with `engine replay --every-seal --log`: plain `engine replay` checks in full only the first
  and last seal of each run. When the error says that an instruction a checkpoint covers carries no
  seal by an accepted signer, an archive mirror or your host altered the copy, which `engine replay`
  accepts. Move the copy aside so that the trading server rebuilds it from the archive, or leave it
  for your turn's whole-log take.
- `read_posted_head` reads the latest posted log head. Check the `LogHeads` reads.
- `pin_posted_head` pins a posted head that the replica passed without keeping. It needs a later
  posted head.

**Your next turn may not be taken until a blocked step works again.**

### What happens at the boundary

This section describes what your trading server does by itself when your turn starts. You do not
need to act unless an alert fires.

A following trading server whose turn is next stops following at the boundary, by its trusted time,
not at the end of a stint. Before it connects to the incumbent, it reads all of these and takes its
trusted time sample:

- the vault, the schedule and the signers;
- the checkpoint anchor at the vault's finalised checkpoint. The incumbent's checkpoint that is
  still in flight is in its copy, and it resumes that checkpoint as outstanding;
- the MONKEY budget and the latest posted log head.

One short stint then brings its copy up to date. It asks the incumbent for the keys and only for the
entries past its copy's tip, takes them onto its copy and its replica, and starts sequencing from
the replica. It neither replays the log nor reads the chain or samples time in between. The switch
therefore costs the incumbent's final checkpoint, the transfer of the tail and the trading server's
start. `engine_full_replays` does not change across the switch.

A copy that is out of date when your turn comes, as after a restart, follows the incumbent until it
is within 4,096 commands of the incumbent's head, for at most ten stints, before its first take.

A trading server started at the boundary with no copy takes a trusted time sample, reads the turn
from the finalised schedule, connects to the incumbent's handover endpoint, and takes the keys and
the log's tail. Neither side waits for chain finality after the boundary.

When the member of the turn before yours never took that turn, your trading server tries each
earlier turn's latest member and source in turn, a week back at most. It then tries the members that
those turns' skips passed over. The operator that still holds the keys hands them over. That
includes an incumbent that stopped for an earlier member whose transfer never confirmed, and that
reports `handover_unconfirmed`. Such an incumbent admits you when the set your turn was drawn from
names your candidate and key with seven days' notice by your boundary. The seven days run from the
first of the unbroken run of sets that named them.

Your trading server's first command in the turn is `TurnTaken`, which starts your time in office for
the operator's share. A turn that you hold again the next day needs no handover, and your trading
server logs `TurnTaken` for it at the boundary. A turn that names you after a turn whose member
never took it needs no handover either, while your trading server still holds the keys. The schedule
gives that turn a new generation, and your trading server seals the keys in it before it logs
`TurnTaken`, so that a restart opens them.

Once a successor confirms, the incumbent's trading server goes back to following, and follows its
successor.

### If the take is refused

Use this procedure when `handover_tip_not_on_log` or `handover_tail_lagged` fires on a member taking
its turn. The alert names the `turn`.

An incumbent that does not find your copy's tip on its log refuses before it stops, and keeps the
keys and sequencing:

- `handover_tip_not_on_log`, a Critical alert, means that your copy diverged from the operator's
  log. The incumbent's log holds another history at that seq, or no entry that far.
- `handover_tail_lagged`, a Warning, means that your copy's tip is older than the 65,536 prefixes
  the incumbent retains, or that the tail is over its bound. The alert's `cause` says which. Your
  trading server catches up and retries.

After three refusals, your trading server sets its copy aside as `<log dir>.aside`, replacing any
earlier one, and takes the whole log as a trading server with no copy does. It does so because the
copy's segment files are its own writer's, not the holder's. An election that names your trading
server, or a turn whose copy does not open, takes the whole log in the same way. Other failures of a
take, such as the chain reads or the trusted time sample before it connects, or no source answering,
are retried for five minutes before the trading server does the same. A passing RPC or Roughtime
outage therefore does not cost the tail.

1. For `handover_tip_not_on_log`, compare the copy set aside in `<log dir>.aside` with the
   incumbent's published log at the tip. **A divergence is a fork on the signing key.** Tell the
   owner.

2. For `handover_tail_lagged`, check your trading server's follow stints, which
   `standby_follow_failed` reports, and its host's load. Your copy fell behind while following.

3. If no earlier member or source hands over the keys, start your trading server with
   `--predecessor <host:port>`, where `<host:port>` is the handover endpoint of the operator that
   holds the keys. On the enclave image, write `<host:port>` to a file named `predecessor` on the
   data disk before you boot the VM, as
   [Upgrade to a new release](deploy-and-rollback.md#upgrade-to-a-new-release) describes. The image
   passes the file's contents as `--predecessor`.

   **Changes the live system.** The owner must approve this start.

**How to tell it is resolved.** Your trading server logs `TurnTaken` and sequences.

**When to escalate.** Tell the owner about every `handover_tip_not_on_log`, with the set-aside copy
kept.

### After your turn

Your key stops signing at the next boundary, and you earn nothing past it, even if your trading
server keeps sequencing because no successor came. The next member's trading server takes the keys
from yours at the boundary. `release_superseded` pages only if the member has not taken them 15
minutes after the boundary. Hand over as
[Handing over to the next operator](#handing-over-to-the-next-operator) describes.

Once a successor confirms, your trading server follows it, and never takes back the generation it
handed over, even when a later trusted time sample falls just before the boundary.

### Withdrawals and the boundary's log check

Use this section when `log_check_pending`, `log_check_unattributable` or `log_check_unavailable`
fires.

While rotation is on and a turn is in effect whose set names two or more members, the operator's
trading server signs a withdrawal only once a check has verified the log through the withdrawal's
approval. An approval therefore waits for the next boundary. A one-member set, or rotation off,
needs no check.

- **At a handover,** the successor's start is the check, because its replica verified the log
  through the checkpoint that it was handed.
- **Your own upgrade's start is no check,** because your member sequenced the range. The approvals
  it holds wait for the next boundary's check, as they would have without the upgrade.
- **At a boundary that you hold on through,** your trading server writes a checkpoint after its
  `TurnTaken`. It then asks the members of the turn's order, first to last, for an attested
  verification of that checkpoint, in questions that your enclave signs. It skips your own
  candidate, members with no handover endpoint, and members that sequenced part of the range since
  the last check under any release, or that run a release that did. It asks again every minute, and
  `log_check_pending` pages after 30 minutes without a verification.

Between your turns, your trading server answers those questions on your registered handover
endpoint, from the checkpoints its replica verified. It answers only when the enclave that signed
the checkpoint also signed the question. **Keep your handover endpoint reachable from every member's
trading server, not only from the next turn's member.**

The verified head is sealed with the keys, and an upgrade carries it to the new release. A start
that finds that its log does not hold the sealed head asks for the check again.

**Before rotation is turned on, every member must run a release that has the log check and that
names the releases that hand the keys on.** Keys that carry a verified head, or those releases, do
not open on an older release. With rotation off, the keys carry neither, as
[Know before you start](deploy-and-rollback.md#know-before-you-start) describes.

1. For `log_check_pending`, check that each member that may answer follows the operator, with no
   `standby_follow_failed` or `member_follow_blocked` open, and that its registered handover
   endpoint is reachable from the operator's trading server. A member answers only once its replica
   has verified the checkpoint that the question names.
2. For `log_check_unattributable`, restart the trading server without `[snapshots]`, so that it
   replays the whole log.

   **Changes the live system.** The owner must approve the restart.

3. For `log_check_unavailable`, restart the trading server with both `operator.rotation_enabled` and
   `[tee.sealing]` set, as [Prepare a member](#prepare-a-member) describes.

   **Changes the live system.** The owner must approve the restart.

**How to tell it is resolved.** The alert resolves once a member verifies the log, or once the check
is no longer due, and the waiting withdrawals are signed.

### A turn the engine refused

Use this section when `turn_refused` fires. It means that no rotation set mirrored at least seven
days earlier names your candidate with your key, so the key in effect stays for that turn. The seven
days run from the first of the unbroken run of mirrored sets that named your candidate with that
key. A set mirrored late that names you unchanged therefore keeps your notice. A new key, or a
return after a set that dropped you, waits seven days again.

**How to tell it is resolved.** The message gives the reason. A turn mirrored after it ended clears
with the next turn. A new or changed key clears for turns that apply seven days after the set that
names it was mirrored.

### Before a new protocol version

A trading server activates its build's newest protocol version when it boots, and a build that
predates a version cannot read a log that activated it. **Move every candidate in the rotation sets
to a release that implements the version before any member boots one.** Otherwise the next member on
an older release cannot take the keys and the log. An incumbent refuses, before it stops, a
successor whose build reads only versions older than its log runs, and keeps the keys.

### Rotation off and failed operators

Turning rotation off stops draws. The turns already drawn stand, and the last member stays named
until the timelock installs an operator once those turns end.

A failed operator is replaced within minutes, as [Minute-scale liveness](#minute-scale-liveness)
describes. The registry's own `promote()` reverts while a schedule is named. Once the turn's order
is exhausted, the governance fallback applies: rotation off, then `installOperator` once the drawn
turns end, as [When no standby qualifies](standby.md#when-no-standby-qualifies) describes.

## Minute-scale liveness

Use this section when you configure a member's liveness caller or standby key, or when an alert in
the table below fires. While a turn names the operator, the log head that the operator's enclave
posts is its heartbeat. A member whose operator misses the heartbeat is promoted at once.

| Alert | Where to go |
| --- | --- |
| `liveness_start_refused`, `liveness_unconfirmed` or `liveness_fenced` | [What the operator's trading server does](#what-the-operators-trading-server-does) |
| `liveness_caller_unfunded` | [Configure the liveness caller](#configure-the-liveness-caller) |
| `standby_unregistered` | [Register your standby key](#register-your-standby-key) |
| `standby_certificates_unreadable` | [Your standby key's certificate](#your-standby-keys-certificate) |
| `promotion_start_refused` or `promotion_reorged` | [A promotion](#a-promotion) |
| `superseded_tail_set_aside` | [A replaced operator coming back](#a-replaced-operator-coming-back) |

### What the operator's trading server does

The operator's trading server posts an `engine.log-head.v2` head through the relayer of
`[log_heads]`, normally twice per `heartbeatInterval`. It posts less often only after a slow chain
read, which delays a post to the next read, at most a sixth of an interval later. The interval is the
schedule's `liveness()`, 60 seconds by default. **The `[log_heads]` relayer must hold enough for two
posts per interval.**

With another member in the turn's set, the trading server signs a head or a checkpoint only at a seq
that another member's enclave confirmed holding. It therefore sequences only while a member follows
it.

- Before it sequences after any start, it asks the other members for their copies.
  `liveness_start_refused` names a member that holds more than its log. The copy that the members
  hold is the history.
- It stops taking commands while its log runs more than one interval past the newest confirmation,
  and raises `liveness_unconfirmed`.
- A checkpoint at its regular pace stops commands for at most 250 ms while it waits for a member to
  confirm its seq, and is skipped if none does. After 30 seconds of skipped checkpoints, the next one
  waits however long a member takes, and the trading server raises `checkpoint_deferred`.
- Through a benchmark-sync hold or a trusted-time halt, it posts held heads at the newest seq that a
  member confirmed, so no promotion follows.
- It stops by its own clock 15 seconds before the earliest block time at which a promotion could
  pass, and raises `liveness_fenced`. It resumes on the first read that moves that time on.

A guardian's retirement of the signer ends no run. The heads stop landing, however, so while a turn
names the operator, the trading server fences about `heartbeatMisses` intervals later, until the
timelock installs a signer.

`GET /v1/operator` names the operator's `enclave` and the vault's `signer` epoch and key. A client
accepts an answer only while they match.

### Configure the liveness caller

Use this procedure for every member, and when `liveness_caller_unfunded` fires.

Between its turns, every member's trading server follows the operator, syncing its copy every 200
milliseconds. It answers on its registered handover endpoint with its copy's status, signed by its
standby key, with the copy's latest entries, and with vouches for other candidates' standby keys.

With `[liveness]`, the trading server also does all of these from the caller account:

- It witnesses each slot.
- It skips a turn once `skipDueAt()` passes, and promotes once `promotionDueAt()` passes.
- It relays the operator's signed heads from its stream when the chain's head is older than five
  sixths of an interval. The operator's own heads are due every half interval, so the caller relays a
  head only when the operator's posts fail or are slow to land.
- For the trading server's whole life, whether following or holding a turn, it fixes the fallback of
  a turn whose draw deadline passed with no relayed seed. The fix is `SeedReceiver.fixFallback`,
  sent as two calls 5 seconds apart, and lets the turn be drawn for the rest of its window.

The caller reads the chain every `liveness.read_interval`, 15 seconds by default, and at least four
times a heartbeat interval whatever that says. The operator's trading server reads the chain for the
liveness rule at the same cadence, and asks the members for their copies every second. The fallback
fixer reads the seed receiver every `liveness.fallback_interval`, 30 seconds by default and at most
that in a production build, and every 5 seconds once anyone has fixed a turn's block, so it records the
block's hash within the 256 blocks it can. The RPC budget in
[feed-and-custody-failures.md](feed-and-custody-failures.md#rpc-budget) counts what these reads cost.

1. Add the section to the configuration:

   ```toml
   [liveness]
   caller = "0x..."        # an account the checkpoint endpoint signs for, apart from every relayer
   min_balance = "0.01"
   read_interval = "15s"      # optional; the default
   fallback_interval = "30s"  # optional; the default
   ```

   `caller` is an account that the checkpoint endpoint signs for. It must differ from the checkpoint
   relayer, the log-head relayer and the seed relayer.

2. Fund `liveness.caller`.

   **Changes the live system.** The owner must approve the transfer.

**How to tell it is resolved.** `liveness_caller_unfunded` pages while the caller holds less than
`liveness.min_balance`, whether the trading server is following or holding a turn. It resolves once
the balance is back above the minimum.

### Your standby key's certificate

With `[api.tls.acme]`, your trading server keeps a public certificate for its standby key's TLS key
while it follows. Its API listener on `node.listen` answers the certificate authority's TLS-ALPN-01
validation of your hostname while it follows. **Forward port 443 to the trading server on a
following member, as on the operator.**

The certificate is sealed apart from your turn's certificate, in `*standby-certificates.json` in the
sealing directory, and is renewed as the API's certificate is. `certificate_pending` and
`certificate_expiring` page for it while you follow. A promotion serves the certificate from its
first answer, so browsers and the other members, which verify the operator against public roots,
follow the promoted member at once.

When `standby_certificates_unreadable` fires, the sealed standby certificate file does not open. The
host altered it, or gave the trading server another release's file. The member stays promotable but
obtains no certificate for its standby key, so a promotion serves the self-signed certificate until
the certificate authority issues one.

1. Delete the `*standby-certificates.json` file in the sealing directory.
2. Restart the member.

   **Changes the live system.** The owner must approve the restart.

### Register your standby key

Use this procedure when `standby_unregistered` fires, and before the timelock turns rotation on.

Your enclave seals a standby key for its release. It asks the operator in effect's API,
`POST /v1/standby/vouch`, and then each member, to vouch for that key. The schedule takes the
registration only from your candidate account, which the trading server does not hold. Once the key
is vouched for, the trading server writes `standby-registration.json` in the sealing directory and
pages `standby_unregistered` with the call. A promoted operator raises the alert too, for the
replacement key that it seals, because the promotion consumed the last key.

**Send the registration before the vouch expires, 30 minutes after it was made.** The trading server
asks for a new vouch 10 minutes before the expiry.

1. Send the registration from the candidate account.

   **Changes the live system.** The owner must approve this transaction.

   ```shell
   cast send $SCHEDULE "$(jq -r .calldata standby-registration.json)" \
     --rpc-url $RPC --account <candidate>
   ```

   The same call, written out, is this:

   ```shell
   cast send $SCHEDULE 'registerStandby(address,uint64,uint64,bytes)' \
     <key> <nonce> <expiry> <vouch> --rpc-url $RPC --account <candidate>
   ```

   `<key>`, `<nonce>`, `<expiry>` and `<vouch>` are the values that the alert names.

2. Confirm that the key is promotable:

   ```shell
   cast call $SCHEDULE 'promotable(address)(address)' $CANDIDATE --rpc-url $RPC
   ```

   The command prints the standby key's address.

**How to tell it is resolved.** `standby_unregistered` resolves once the key is promotable. A key
that the schedule never took is kept across restarts, and sealing a new key removes the file.

Register the key before the timelock turns rotation on. Start your member's trading server with its
rotation configuration while the switch is still off. The trading server prepares the key, asks the
deployment's operator to vouch for it, and waits for the switch.

**A candidate with no promotable key at an election close is not adopted by its votes, and its votes
do not count towards the quorum.** A member of the latest set keeps its place without a key for
about an epoch, as [Daily turns](#daily-turns) describes. A `setRelease` voids the key, and the
trading server registers a new key for the new release. Move releases early enough in an epoch for
the new key to register before the epoch's close.

### A promotion

Once `heartbeatMisses` witnessed slots, 5 by default, pass with no head, anyone's `promote()` names
the next qualified member. The vault moves its signer to that member's standby key, in a new signer
epoch. The promotion names the next member in the turn's order that never received the keys of the
turn, whether or not it was drawn or skipped to, and takes the key holder last. A member that held
the keys under an assignment of the turn is never named again in that turn. To read whether a member
held the keys, run:

```shell
cast call $SCHEDULE 'heldKeys(uint64,address)(bool)' <turn> <candidate address> --rpc-url $RPC
```

The promoted trading server first takes what any member holds past its copy. It asks the member that
confirmed the head, then every other member, and then the outgoing operator's archive. It starts
sequencing from its copy at once if the copy holds the outgoing epoch's latest head and the vault's
latest checkpoint that the promotion fixed. `promotion_start_refused` pages a copy that cannot
start, and the next promotion passes over that member.

The promoted trading server records the promotion in `promotion.json` in the sealing directory. A
restart resumes from the copy while the chain records the same promotion, and never cuts the copy
back to the vault's anchor. Its checkpoints and payouts wait for the promotion's finality.

When `promotion_reorged` fires, the latest block no longer records the promotion, and the trading
server has stopped signing.

1. Restart the trading server.

   **Changes the live system.** The owner must approve the restart.

   The trading server waits, signing nothing, until a final block settles the promotion. It resumes
   if the final block records the promotion. Otherwise, it sets the copy, the resealed keys and the
   record aside, renaming each to `*.quarantined-<turn>-<position>-<block time>` beside where it
   was. It then pages `promotion_reorged` again, and follows as a member with a new standby key.

2. Keep the set-aside files for the owner.

### A turn whose member does not take it

A member takes its turn with its first head under its assignment, which `LogHeads` latches. **Post
the first head as soon as the trading server holds the keys.**

`takeWindow` witnessed slots, 2 by default, after the latest of the boundary's slot, the
assignment's slot and the last signer change's slot, are the window for that latch. To read the
window, run:

```shell
cast call $SCHEDULE 'takeWindow()(uint16,uint16,uint16)' --rpc-url $RPC
```

The command prints the window in slots, followed by its minimum and maximum.

Once the window passes with no latch, anyone's `skip()` names the next listed member in the turn's
order that has held no assignment of the turn, other than the key holder. The skip creates a new
generation, with no signer change. The key holder is the candidate whose enclave holds the signer
epoch's keys. It is the candidate whose assignment, release move or naming as a turn's source recorded
the release that signed the newest head, whatever release it has moved to since, when that head was
signed since the latest turn drawn while the registry named its own operator began. Before such a
head, the keys are where the epoch found them. To read the key holder, and that turn, run:

```shell
cast call $LOG_HEADS 'keyHolder()(address)' --rpc-url $RPC
cast call $SCHEDULE 'registryTurn()(uint64)' --rpc-url $RPC
```

The first command prints zero when nothing names a holder, as for a newest head whose release no
candidate recorded.

- An unwitnessed slot, as during a chain outage, restarts the window.
- A promotion's assignment is never skipped.
- No skip is due while the guardian has retired the signer.

Skips act at sequencer confirmation, not at finality. The member named takes the keys, and the key
holder admits it, from the turn's assignment at the custody quorum's latest agreed block. The
holder's log must show the skip's set naming the member with seven days' notice. A skip's member
that upgrades to another approved release hands its keys on only once the log has mirrored the skip
at finality.

Whoever holds the keys hands them on. That is the incumbent, or a member that took them late and was
then skipped, which starts, sequences and hands them to the member now named. A holder whose skip a
reorg removed hands them to the member that the schedule names again.

- A member is found where its heads are newest, then among the turn's other assignments, and then at
  the turn's source and earlier holders.
- A following member moves to the holder when the operator that the registry names fails a stint.
- A skip's member with no copy of the log, or one restarting, reads the skip at its start in the
  same way, before the skip is final.
- A member that handed its drawn turn to a skip's member takes the turn back once a reorg removes
  the skip.
- A holder that the registry no longer names, as one that a later skip or a reorg moved past, does
  not restart as a holder, as an incumbent past its boundary does not. It follows. **Leave its
  sealed keys alone,** because a later skip or the missed-head promotion moves the turn on.

If no assignment of a turn is latched once `heartbeatMisses + 1` witnessed slots pass after the
later of the boundary's slot and the last signer change, in a set of two or more members,
`promote()` passes even while the incumbent posts heads. Those heads bind the incumbent's own
earlier turn. The promotion names the next qualified member, and names the incumbent, which holds
the keys, last. It names the incumbent's own standby key only when no other member qualifies. The
incumbent stops by its own clock before that, because `promotionDueAt()` counts this path too. With
no member qualifying, the promotion is refused after the incumbent has stopped. Then use the
governance fallback: rotation off, then `installOperator`, as
[When no standby qualifies](standby.md#when-no-standby-qualifies) describes.

The timelock sets the take window within its deployment bounds, with `setTakeWindow`, while no turn
names the operator. These bounds hold:

- The deployment's minimum window must exceed the switch budget, plus one interval, plus a head's
  inclusion time (`headTolerance`), at the shortest interval that the bounds allow.
- The switch budget covers the daily rotation scenario's measured switch, plus two reads of the
  turn's latest assignment after the stop, one by the holder and one by the successor. Each read is
  ten round trips to the custody providers, one after another, or eleven while a provider's tip is
  past the agreed block. At 100 milliseconds a round trip, the reads add up to 2.2 seconds to the
  measured switch.
- The schedule refuses a take window, through `setTakeWindow`, or a heartbeat rule, through
  `setLiveness`, that would leave the first skip's member no such take before the untaken-turn
  promotion. `(heartbeatMisses - takeWindow) * heartbeatInterval` must exceed the switch budget plus
  the larger of `headTolerance` and the 15 seconds before a promotion at which the member's trading
  server stops signing. To lengthen the window past that, raise `heartbeatMisses` first.

### A replaced operator coming back

Use this section when `superseded_tail_set_aside` fires. The outgoing operator, whether restarted or
still running, follows the promoted member. Its log may hold commands past the last head that its
signer epoch posted, which no member received. Once a final block shows that epoch ended, the
trading server sets its log aside as `<log_dir>.superseded-<seq>`, pages `superseded_tail_set_aside`
with the commands that it dropped, and rebuilds its copy from the promoted member's archive.

1. Keep the set-aside log for the owner. Clients may have seen answers for the dropped commands.

### Assumptions the rule rests on

- Arbitrum's block timestamps lead true time by no more than 10 seconds, which is the first part of
  the stop margin.
- The enclave's clock runs no more than 500 parts per million slow.
- A sequencer outage produces no blocks, so no slot is marked and no promotion passes. The operator
  stops about `heartbeatMisses` intervals after its last read, and resumes on its first read after
  the outage.
- The delayed inbox and forced inclusion take hours, so nothing in the rule relies on them.
- `headBlocks` must span less than one interval at the block rate that the deployment pins, which is
  200 blocks at 250 milliseconds.

## Parameter votes

Use this section when the rotation set must change a governed parameter, a listing, a close-spread
adoption, the access policy or a protocol activation, or when `proposal_refused` or
`proposal_rejected` fires.

With `[custody.evm.staking.registry.vote]` naming the `ParameterVote` contract over the schedule,
the members of the rotation set in effect vote on parameter changes while rotation is on. Votes
govern while both of these hold:

- the registry's `rotationEnabled()` is true;
- a rotation set with members is in effect.

To check the second condition, run:

```shell
cast call $SCHEDULE 'currentSetId()(uint32)' --rpc-url $RPC
```

The command prints a nonzero id while a set is in effect, and `setOf` with that id lists its
candidates. Members can propose only while votes govern. From the start of rotation until the first
set takes effect, seven days plus the finality margin after the close that adopts it, and while a
set with no members is in effect, the operator signs the governed commands within its own bounds, as
with rotation off. A drawn turn keeps `rotationEnabled()` true from its boundary after the switch
turns off, until an installation. The trading server mirrors the registry's named turn with each
finalised staking frontier.

While votes govern, the turn's operator keeps signing benchmark rates, its own stress floor, which
is at most level 1 and lapses after 72 hours, and reduce-only. The engine refuses these when the
operator signs them, and they need a vote:

- `SetParams` and `UpsertMarket`;
- the parameter, markup and listing notices;
- close-spread adoptions;
- corporate actions, as [Corporate actions](corporate-actions.md) describes;
- the trading server's own protocol activation and access policy.

A relisting that a runbook asks for, such as a futures contract, a price rule or a calendar, or a
corporate action, is then a normal vote. Propose it at least seven days before it must apply, plus
the window it takes to pass. Propose a corporate action two trading days earlier still.

### Before you propose

- **A command that a release added,** such as a protocol activation or a new field, needs every
  member on a release that knows it. A trading server whose release cannot decode a proposal's bytes
  mirrors the proposal as no command, and the engine refuses it.
- **An access policy** needs every member on a release whose configuration's `[access]` section
  names exactly that policy, with its `source`, and a `max_age` equal to its `max_age_ms`. Once the
  vote applies, the log records the policy. A trading server whose configuration names no policy, or
  another, then refuses to start on that log with
  `the configured access policy differs from the public log`. A member on an older release cannot
  take its turn, and the incumbent holds on until that member's release is approved and deployed.

### Propose and vote

1. Write the command as the compact JSON that the engine writes for it. That is the `command` of the
   request that `engine admin` prints, with no spaces, fields in the engine's order, and defaults
   left out. **The engine refuses any other encoding of the same command.** A `SetParams` or
   `UpsertMarket` names every field. Take the values in force, with the baseline from
   `GET /v1/house/state`'s `governance` section where a restriction holds one, and change only what
   the vote is for.

2. Propose the command, from a member's candidate account.

   **Changes the live system.** The owner must approve this transaction.

   ```shell
   cast send $VOTE 'propose(bytes,bool)' 0x<hex of the JSON> <fast> \
     --rpc-url $RPC --account <candidate>
   ```

   `<hex of the JSON>` is the command's JSON in hexadecimal, and `<fast>` is `true` for the fast
   path and `false` for a normal change. The proposal casts the proposer's yes vote.
   `GET /v1/operators` lists the open proposals with their weights, and shows `command: null` for a
   proposal whose bytes are not such an encoding.

3. Each member of the set that the proposal opened under votes, until the window closes.

   **Changes the live system.** The owner must approve this transaction.

   ```shell
   cast send $VOTE 'vote(uint256)' <id> --rpc-url $RPC --account <candidate>
   ```

   `<id>` is the proposal's id. The window is `normalWindow()` or `fastWindow()`. The proposal
   passes when its yes weight is more than two thirds of the set's weight.

**How to tell it is resolved.** A normal change appears in `GET /v1/house/state` under
`governance.pending`, with `applies_at`, and applies at the trading server's first command from
then. A fast-path change appears under `governance.restrictions`, with its `expires_at`.

**When to escalate.** `proposal_refused` means that the engine refused the proposal, for the reason
in the message. `proposal_rejected` means that a passed normal change failed its checks when it came
to apply, for the reason in the message. Propose it again over the parameters in force, and tell the
owner.

### A normal change

A normal change needs no notice of its own, because its seven days are the vote's. It takes effect
seven days after the engine logs it. It meets every other bound that the operator's command would
meet:

- a relisting keeps each changed spread band between its launch rate and twice that rate;
- a relisting keeps each changed margin requirement at or above the launch requirement;
- a relisting keeps its market's markout floor;
- new parameters keep every size multiplier at 1x or more;
- new parameters keep each toxicity score tier within 50% of its launch score and add-on;
- the profit fee base stays at or above its launch value, and the fee curve at or above its launch
  curve;
- a close-spread ceiling covers its market's launch close charge.

A normal change applies only over the baseline it passed on. A `SetParams` or `UpsertMarket` whose
parameters or market changed in the meantime is refused rather than undo that change, whether
another vote or, while votes do not govern, the operator made it. A ceiling adoption is refused
likewise while another adoption waits. **A change that applied while a proposal was still open is
not caught in that way. The proposal carries every field, so check it against the values in force
before you vote for it.**

### The fast path

The fast path takes only these changes, with every other field as the baseline sets it:

- a higher margin band or close-out level;
- a stress floor at any level;
- a lower house cap or market cap;
- reduce-only.

A fast-path change applies at once, as a restriction listed under `governance.restrictions` with its
`expires_at`. A fast-path vote on the same command, or on one that holds the same bounds, renews it.
Nothing lifts a restriction early, and new parameters that drop a stress level a restriction holds a
floor at are refused. **A margin raise has no range beyond each band's validation, and closes out at
once the positions it leaves short of margin.**

## Operator faults

Use this section to prove that a member's enclave signed two histories, or when
`operator_fault_refused` fires. A member whose enclave signs two histories during its turn, as one
whose host rolls it back over the unposted window does, can be proven at fault.

**Never restart your enclave from an older copy of its log during a turn.** A rollback within the
unposted window is the fault that this proves, whatever the host meant by it.

Your configuration names the same `[custody.evm.staking.registry.fault_proof]` contract and penalty
as every member's configuration. The log records them once protocol version V70 is in force. A
trading server whose configuration names none, or others, refuses to start on that log with
`the log records fault proof contract`, so your turn would not start.

1. Take two of the member's `engine.log-head.v2` heads that are of one signer epoch, bind the same
   turn, position and release, and name different links at one seq. The signed stream carries every
   head, and followers relay them.

2. For each head, find the index of the assignment's revision that the head binds. `revisionOf`
   reads the revision stored at one index of a turn's position:

   ```shell
   cast call $SCHEDULE 'revisionOf(uint64,uint16,uint256)' <turn> <position> <index> --rpc-url $RPC
   ```

   The command prints the assignment's candidate and the revision stored at `<index>`.

3. Prove the fault from any funded account.

   **Changes the live system.** The owner must approve this transaction.

   Call `FaultProof.prove(epoch, a, b)` with each head as `LogHeads.postV2` takes it, its signature,
   and the index of the assignment's revision that it binds.

**How to tell it is resolved.** `FaultProven` names the member, once per assignment. The engine then
cuts that member's share of the operator's earnings by a haircut that starts at the recorded penalty
and falls linearly to nothing over its decay. The withheld part goes to stakers.
`GET /v1/house/state`'s `operator_faults` section shows the penalty and when each haircut started,
and the member's history lists each `operator_share_withheld`.

A fault that the engine mirrors before protocol version V70 is in force on the log, or before the
log records the fault proof contract, is credited but cuts nothing, and `operator_fault_refused`
fires.
