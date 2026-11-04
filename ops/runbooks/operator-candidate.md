# Standing as an operator candidate

The operator runs the enclave host, holds the admin key, and is chosen on chain: stakers delegate
their MONKEY to a candidate, and anyone may close an election epoch, which elects the eligible
candidate with the most delegated stake. Rotation is **off at launch**. The deployer is the only
operator until the timelock turns it on, and until then none of the election steps here has an
effect. This runbook has four parts: standing for election, becoming the operator when elected,
handing the node to the next operator, and upgrading while you are the operator. Running as a
standby is in [standby.md](standby.md). Read [reference.md](reference.md) first.

The registry's launch parameters (#360): epoch 14 days, minimum term 90 days, finality margin 1 day,
challenger margin 10% of the incumbent's votes, eligibility 1% of total stake, quorum 25% of total
stake, candidate cap 25, minimum self-bond 0.1% of MONKEY's maximum supply, liveness timeout 3 days.
The deployed values are the ones that count: `cast call $REGISTRY
'parameters()(uint64,uint64,uint64,uint16,uint16,uint16,uint16,uint256,uint64)' --rpc-url $RPC`
prints, in order, the epoch length, minimum term, finality margin, margin, eligibility and quorum
(basis points), candidate cap, minimum self-bond, and liveness timeout.

## What a candidate needs

| Need | Detail |
| --- | --- |
| A bare-metal AMD SEV-SNP host | Its own, with the image's launch flags ([deploy-and-rollback.md](deploy-and-rollback.md)) and a data disk. |
| A hostname and a CAA record | The candidate's `apiHostname`, with DNS, port 443 forwarded unmodified to the node, and a CAA record naming the chosen CA and TLS-ALPN-01 ([certificate-renewal-failure.md](certificate-renewal-failure.md), "Before the first certificate"). The enclave orders its certificate for this name under the ACME account that moves with the keys. |
| Its own custody providers | At least three, run by different operators, serving archive state back to the vault's deployment block. The quorum that credits deposits is the candidate's own choice, which is why the timelock approves each release. |
| Its own market-data accounts and secrets | The venues and vendors in its config, with their API keys. No secret moves from the incumbent: the candidate delivers its own to its own enclave ([key-recovery.md](key-recovery.md), "Secrets"). |
| A checkpoint endpoint and a funded relayer | Its own, per [feed-and-custody-failures.md](feed-and-custody-failures.md), "Checkpoint endpoint". |
| An alert receiver | `[alerts.critical]` is required with real attestation. |
| An admin key | Made and kept offline, ideally on a hardware wallet. It signs `engine admin` commands and its registration. |
| A candidate account | An Arbitrum account holding the self-bond in staked MONKEY and ETH for gas. It may differ from the admin key. While its candidate is the operator in effect, it also moves the vault's release to any release the timelock approved for it (section 5), and the operator in effect cannot deregister to revoke it: keep it like the admin key, offline, on a hardware wallet or a multisig. |
| A Certificate Transparency watcher | On a host independent of the node's, started before its first certificate. |
| An archive mirror | At least one store that does not share a failure with the node. A successor reads the log from mirrors when the node is dead. |

## The candidate's config

The candidate's node must start on the **log the incumbent wrote**, so everything the log records
must match, and everything else is the candidate's own. Start from [production-node.toml](production-node.toml).

| Must equal the running log | The candidate's own |
| --- | --- |
| `[genesis]`: the launch genesis, close-spread ceilings and staking included | `node.listen`, `[api]` limits and origins, `[api.tls.acme]` (its hostname, contact) |
| `node.auth = "eip712"`, `custody.chain_id`, `custody.vault` | `[custody.evm]` providers and their credentials, `[checkpoints]` endpoint and relayer |
| `[custody.evm.staking]` contract and registry addresses | `[feeds]` venues, vendors, symbols and the names of their secrets |
| `[access]` `source` and `max_age` (recorded once in the log) | `[time]` servers, `[metrics]`, `[alerts]`, the `[access]` endpoint |
| | `operator.public_key`: **the candidate's own admin key** (the log records the incumbent until the election takes effect, and a config naming the key of a pending election on the same chain and vault is accepted); `[tee.sealing]` directory and the handover listener |

A start whose config differs from what the log records is refused with `log_recovery_failed`. Its
config hash is what the timelock approves, so every change after approval is a new approval.

## 1. Stand for election

1. **Build the release** (candidate's image and config) and publish it, as in
   [deploy-and-rollback.md](deploy-and-rollback.md), section 1. You need `$DIGEST` (the 48-byte
   launch measurement) and `$CONFIG_HASH`.
2. **Stake.** The self-bond comes from MONKEY the candidate account has staked. `$STAKING` is the
   registry's staking contract.

   ```sh
   STAKING=$(cast call $REGISTRY 'staking()(address)' --rpc-url $RPC)
   MONKEY=$(cast call $VAULT 'monkey()(address)' --rpc-url $RPC)
   cast send $MONKEY 'approve(address,uint256)' $STAKING $AMOUNT --rpc-url $RPC --account <candidate>
   cast send $STAKING 'stake(uint256)' $AMOUNT --rpc-url $RPC --account <candidate>
   cast send $STAKING 'delegate(address)' <candidate address> --rpc-url $RPC --account <candidate>
   ```

   **Changes the live system.** Needs the owner's approval (these are the candidate's own funds and
   transactions; the owner decides whether this party may stand). `delegate` to itself is optional;
   votes may come from any staker. Check: `cast call $STAKING 'stakes(address)(uint256)'
   <candidate address> --rpc-url $RPC` is at least the bond, and the minimum self-bond is below it.
3. **Authorize the key.** The admin key signs an EIP-712 authorization of this candidate, so no one
   can register a key the candidate did not choose. The registry computes the digest; the admin key
   signs it as a hash, without the Ethereum message prefix:

   ```sh
   DIGEST_REG=$(cast call $REGISTRY 'registrationDigest(address,address)(bytes32)' <candidate address> <admin key address> --rpc-url $RPC)
   SIG=$(cast wallet sign --no-hash $DIGEST_REG --ledger)          # or --account <keystore>
   ```
4. **Register.** From the candidate account. `enclave` is the address a standby of this release would
   register; it matters only to a takeover after a liveness promotion and is replaced then by
   `setEnclave` ([standby.md](standby.md)). Use the address of a rehearsal boot of this release, or
   any address you control. `bond` is in MONKEY base units; the hostname and handover endpoint are
   text (`host:port` for the endpoint: the address a successor reaches `tee.sealing.handover.listen`
   on).

   ```sh
   cast send $REGISTRY 'register((address,address,uint256,bytes,bytes32,string,string),bytes)' \
     "(<admin key>,<enclave>,$BOND,$DIGEST,$CONFIG_HASH,api.candidate.example,candidate.example:9471)" $SIG \
     --rpc-url $RPC --account <candidate>
   ```

   **Changes the live system.** Needs the owner's approval of this transaction. It locks the bond.
   Errors: `AlreadyRegistered`, `BondBelowMinimum`, `AdminKeyTaken`, `UnauthorizedAdminKey` (the
   signature is not the admin key's over this candidate), `InvalidEndpoint`.
   Check: `cast call $REGISTRY 'candidateOf(address)((address,address,uint256,bytes32,uint64,string,string))'
   <candidate address> --rpc-url $RPC` prints the registration, and `release` equals
   `cast keccak $(cast concat-hex $DIGEST $CONFIG_HASH)`.
5. **Get the release approved.** The timelock calls `setReleaseApproval(candidate, digest, configHash, true)`
   (pattern in reference.md). The owner reviews the published config (providers, vendors,
   hostname) first: this approval is what makes the candidate eligible.

   ```sh
   TARGET=$REGISTRY
   DATA=$(cast calldata 'setReleaseApproval(address,bytes,bytes32,bool)' <candidate address> $DIGEST $CONFIG_HASH true)
   ```

   **Changes the live system.** Needs the owner's approval of this contract call.
   Check: `cast call $REGISTRY 'isEligible(address)(bool)' <candidate address> --rpc-url $RPC`
   prints `true` once the candidate also has the eligibility share of total stake in votes
   (`cast call $STAKING 'votes(address)(uint256)' <candidate address>`). Withdrawing the approval
   delists it, until the candidate moves to another release still approved for it (section 5): to
   keep a candidate off the list, withdraw every release approved for it.
6. **Run the standby** ([standby.md](standby.md)) so the candidate is ready for a promotion. A
   candidate that wins an election instead takes the keys by handover (below).

A candidate cannot leave while it is the operator in effect, or elected and waiting. Otherwise
`cast send $REGISTRY 'deregister()'` removes it and frees the bond one cooldown later. A candidate
changes its release without leaving, as in section 5, steps 2 and 3: an election names the release
the candidacy holds when the epoch closes.

## 2. The incumbent registers too

The election hands the keys to the winner **from the incumbent's registered endpoint**. The
registry records the operator in effect's handover endpoint and API hostname when an election is
made. The initial operator, set at deployment, is not registered until it registers. If it has not,
the successor finds no endpoint and refuses to start (`handoverSource()` is empty). So, **before the
owner turns rotation on**, the initial operator registers as a candidate from its admin key's own
address (the initial operator's candidate identity is its admin key), with its real hostname and
handover endpoint and a release the timelock approves. Use steps 2 to 5 above with the admin key as
the candidate account.

Registering does not change the release the registry names for the operator in effect: until it
moves, that is the release the deployment named, and it is what the vault names once rotation is
on. So, before rotation turns on, register with the release the vault names now and move the
candidacy to it (section 5, step 3, with that release: a move to the release the candidacy holds
keeps its approval), and check that the two agree:

```sh
cast call $REGISTRY 'operatorInEffect()((address,address,bytes32,uint64))' --rpc-url $RPC   # the third field
cast call $VAULT 'currentRelease()(bytes32)' --rpc-url $RPC
```

Otherwise turning rotation on makes the vault name another release, and the running node's keys
no longer open on a restart (`release_superseded`).

Check, once an election exists: `cast call $REGISTRY 'handoverSource()((address,bytes32,string,string))'
--rpc-url $RPC` names the incumbent, its release, hostname and endpoint.

## 3. Becoming the operator

After an election, the sequence is fixed by the contracts. Times are the registry's, in seconds.

1. Someone calls `closeEpoch()` after the epoch ends (anyone may; it is a plain transaction):
   `cast send $REGISTRY 'closeEpoch()' --rpc-url $RPC --account <any>`. It elects only when rotation is
   on, no election is pending, the quorum is delegated, the incumbent has served its minimum term, and
   the winner's votes beat the incumbent's by the margin. Errors: `EpochNotEnded(<time>)`. **Changes
   the live system.** Needs the owner's approval to send it.
2. The election takes effect after the notice (7 days) plus the finality margin (1 day).
   `cast call $REGISTRY 'pendingElection()((uint64,address,address,bytes32,uint64,bool))' --rpc-url $RPC` shows it:
   epoch, candidate, admin key, release, `effectiveAt`, and whether it is a liveness promotion.
3. The incumbent's node raises `operator_elected` (Warning) when the engine mirrors the election.
   The engine applies your admin key from `effectiveAt`, and the vault names your release then.
4. **Prepare your node before `effectiveAt`.** A successor needs: its disk with `config.toml` (the
   approved release, byte for byte), with `mirrors` under `[tee.sealing.handover]` set to your archive
   mirrors of the incumbent's log, and nothing else: no `new-key`, no `predecessor`. The disk must
   hold no sealed keys for this release in the vault's generation of it. Have your own secrets in a
   bundle file on your workstation: nothing comes from the incumbent.
5. **After `effectiveAt`**, once the block is `finalized`, fold the release change into the vault's
   generation (anyone may; your node waits for it up to a minute, then refuses):

   ```sh
   cast send $VAULT 'syncRelease()' --rpc-url $RPC --account <any>
   ```

   Check: `cast call $VAULT 'currentRelease()(bytes32)' --rpc-url $RPC` is your release's id, and
   `releaseGeneration()` rose.
6. **Start your node.**
   **Changes the live system.** Needs the owner's approval to deploy this release and to take the
   incumbent's keys.
   It holds first, with `secret_missing` and only its delivery listener up. Deliver your bundle, under
   your admin key ([key-recovery.md](key-recovery.md), "Secrets"), and it goes on.
   On the console you will see: no sealed keys, rotation on, the incumbent resolved from the registry
   (`election handover: …`), the incumbent's published log fetched from its hostname or your mirrors, and
   the keys taken from its handover endpoint. The incumbent checks your quote against the vendor's chain
   and the vault's named release, stops sequencing, and sends the keys and the log tail; you confirm and
   it acknowledges and powers off.
   Check: your node serves `/v1/markets`; `curl -sf $API/v1/operator | jq .operator` names your admin
   key's address (from `effectiveAt`); the enclave address and the signer epoch are unchanged; the
   incumbent's VM is off; `latestSeq()` rises.
7. If it fails, read [deploy-and-rollback.md](deploy-and-rollback.md), section 3, step 5: the rules are
   the same. Never stop the incumbent while it reports `handover_unconfirmed`.

## 4. Handing the node to the next operator

You are the incumbent and an election has named another operator.

1. When the engine mirrors the election, your node raises `operator_elected` (Warning). Nothing needs
   stopping. Your job is to stay up and reachable until the successor has the keys.
2. Check: your handover listener (`tee.sealing.handover.listen`) is reachable from the internet at the
   `handoverEndpoint` you registered, your API hostname still resolves, and your archive is served
   (`curl -sf $API/v1/archive/manifest.jsonl | head -1`). The successor fetches the published log from
   the hostname or its mirrors, and the log's tail segment from the handover.
3. At `effectiveAt` the vault names the successor's release. `release_superseded` fires on your node
   (expected). The successor connects, your node hands over and powers off.
4. If the successor has not connected by `effectiveAt`, your node raises `handover_overdue` (Critical).
   It still holds the keys and keeps sequencing. Contact the successor and check the same three things
   from its side. Do not stop your node.
5. When the election is cancelled (the timelock turned rotation off, or an election naming you
   again), both alerts resolve.
6. After the handover your admin key signs nothing from `effectiveAt`. Keep the log copy and the
   attestation of your enclave: the log's earlier checkpoints verify only against it.

## 5. Upgrading while you are the operator

While rotation is on, the vault's `setCurrentRelease` reverts (`RotationActive`): the vault names
the operator in effect's release. You upgrade by moving your candidacy to a release the timelock has
approved for it; the vault names the new release at once, and the running release hands it the keys
and the log as in an ordinary upgrade. The timelock's delay still stands between any release and the
keys, since only the timelock approves releases, and a move is one-way: it withdraws the approval of
the release it leaves, so moving back needs a new approval. The running release is `A`, the new one
`B`.

1. **Prepare `B`** ([deploy-and-rollback.md](deploy-and-rollback.md), section 1). Keep the
   hostname and handover endpoint you registered: the registry has no call that changes them while
   you are the operator, so `B` serves them once `A` has powered off.
2. **Get `B` approved for your candidacy**, as in section 1, step 5, with `B`'s digest and config
   hash.
   **Changes the live system.** Needs the owner's approval of this contract call.
3. **Move your candidacy to `B`.** From the candidate account:

   ```sh
   cast send $REGISTRY 'setRelease(bytes,bytes32)' $DIGEST $CONFIG_HASH --rpc-url $RPC --account <candidate>
   ```

   **Changes the live system.** Needs the owner's approval of this transaction: from it, a restart
   of `A` finds no keys, and `A`'s approval is gone (`ReleaseApprovalChanged(<candidate>, <A's
   release>, false)`). Errors: `NotRegistered`; `ReleaseNotApproved(<release>)` (step 2 has not
   executed); `ElectionPending` (an election or promotion that replaces you or names you waits to
   take effect: upgrade after it takes effect, or let the successor run its own release);
   `CandidateCapReached` (your candidacy was delisted and the list is full, past the one place the
   operator in effect may take beyond the cap; after an installation, the operator it replaced may
   hold that place, and the timelock frees one by withdrawing an approval or raising the cap).
   Check: the third field of `operatorInEffect()` and `cast call $VAULT 'currentRelease()(bytes32)'`
   both equal `cast keccak $(cast concat-hex $DIGEST $CONFIG_HASH)`.
4. **Fold the change into the vault's generation** (anyone may; `B` waits for it up to a minute,
   then refuses, and `A` hands over no keys until it lands):

   ```sh
   cast send $VAULT 'syncRelease()' --rpc-url $RPC --account <any>
   ```

   Check: `releaseGeneration()` rose by one. `A` raises `release_superseded` (Critical) once the
   block is `finalized`, and keeps sequencing; the page is expected.
5. **Start `B` with a `predecessor` file**, as in [deploy-and-rollback.md](deploy-and-rollback.md),
   section 3, steps 3 to 7. The enclave address, the signer epoch and `$PIN` carry over. Do not
   start `B` without the file: with no sealed keys and no flag, a node takes the keys from the
   operator the last election replaced (`handoverSource()`), not from `A`.

To roll back before `B` takes the keys, the timelock approves `A` again (section 1, step 5), then
move the candidacy back to `A` (steps 3 and 4) and make `A` restart-safe again as in
deploy-and-rollback.md, section 4. A candidate that is not the operator moves its release the same
way, steps 2 and 3, with no effect on the vault.

An operator the timelock installed ([standby.md](standby.md), "When no standby qualifies")
upgrades the same way once it is a registered candidate: register first (section 1), since
`setRelease` reverts `NotRegistered` for an operator with no candidacy. Until its first move, the
registry names the release the installation named, and that move withdraws the approval of both
that release and the candidacy's. Releases the timelock approved for the candidacy before the
installation stay approved, so withdraw any the operator should not move to. The installation
changes no candidacy: the operator it replaced keeps its release, approvals and listing, and its
moves no longer reach the vault.

## Open questions

These are gaps in the contracts as written. Do not work around them on a live system; the owner
decides.

- **The first election needs a registered incumbent** (section 2). The contracts accept an election
  that leaves `handoverSource()` empty, and the successor then cannot start.
