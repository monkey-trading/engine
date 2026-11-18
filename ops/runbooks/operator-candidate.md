# Standing as an operator candidate

The operator runs the enclave host, holds the admin key, and is chosen on chain: stakers delegate
amounts of their MONKEY to candidates, and anyone may close an election epoch, which elects the eligible
candidate with the most delegated stake. Rotation is **off at launch**. The deployer is the only
operator until the timelock turns it on, and until then none of the election steps here has an
effect. This runbook has four parts: standing for election, becoming the operator when elected,
handing the node to the next operator, and upgrading while you are the operator. Running as a
standby is in [standby.md](standby.md). Read [reference.md](reference.md) first.

The registry's launch parameters (#360): epoch 14 days, finality margin 1 day, eligibility 1% of
total stake, quorum 25% of total stake, candidate cap 25, minimum self-bond 0.1% of MONKEY's maximum
supply, liveness timeout 3 days. The deployed values are the ones that count: `cast call $REGISTRY
'parameters()(uint64,uint64,uint16,uint16,uint16,uint256,uint64)' --rpc-url $RPC` prints, in order,
the epoch length, finality margin, eligibility and quorum (basis points), candidate cap, minimum
self-bond, and liveness timeout.

The launch registry names a schedule (`OperatorSchedule`), so it rotates the operator daily instead
of electing one: section 6 describes the turns. Sections 1 to 5 still describe registering, the
handover and upgrading, which every turn uses.

## What a candidate needs

| Need | Detail |
| --- | --- |
| A bare-metal AMD SEV-SNP host | Its own, with the image's launch flags ([deploy-and-rollback.md](deploy-and-rollback.md)) and a data disk. |
| A hostname and a CAA record | The candidate's `apiHostname`, with DNS, port 443 forwarded unmodified to the node, and a CAA record naming the chosen CA and TLS-ALPN-01 ([certificate-renewal-failure.md](certificate-renewal-failure.md), "Before the first certificate"). The enclave orders its certificate for this name under the ACME account that moves with the keys. |
| Its own custody providers | At least three, run by different operators, serving archive state back to the vault's deployment block. The quorum that credits deposits is the candidate's own choice, which is why the timelock approves each release. |
| Its own market-data accounts and secrets | The venues and vendors in its config, with their API keys. No secret moves from the incumbent: the candidate delivers its own to its own enclave ([key-recovery.md](key-recovery.md), "Secrets"). |
| A checkpoint endpoint and a funded relayer | Its own, per [feed-and-custody-failures.md](feed-and-custody-failures.md), "Checkpoint endpoint". |
| A seed relayer | Ethereum providers, a beacon node, and a funded relayer key apart from the checkpoint relayer's, per [seed-relay.md](seed-relay.md). |
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
   cast send $STAKING 'delegate(address,uint256)' <candidate address> $AMOUNT --rpc-url $RPC --account <candidate>
   ```

   **Changes the live system.** Needs the owner's approval (these are the candidate's own funds and
   transactions; the owner decides whether this party may stand). New stake is undelegated until a
   `delegate` gives it a candidate. Delegating to itself is optional, and any amount up to the
   undelegated stake may be delegated; votes may come from any staker. Check: `cast call $STAKING
   'stakes(address)(uint256)' <candidate address> --rpc-url $RPC` is at least the bond, the minimum
   self-bond is below it, and `cast call $STAKING 'delegationsOf(address)((address,uint256)[])'
   <candidate address> --rpc-url $RPC` lists the delegations made.
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

A candidate cannot leave while it is the operator in effect, or elected and waiting, nor, while a
turn names the operator or a drawn turn runs, while the registry's `holdsTurn` names it: it has held
any assignment of the turn in effect or of the last drawn turn, its release signed the newest head,
or it may hold the keys the turn in effect's member takes: as a member of the turn drawn before it
(only that turn's last member once a head latched it or a promotion named it), and, while no member
of a turn took the keys, of the turns before it, up to seven turns back; or as the source of the
earliest turn so reached, when it is the first turn, the first after an installation, or seven turns
back. A holder is free from the boundary of the turn after the one whose member took its keys,
except as the source of the turn seven turns back, which stays until a later turn starts or an
installation ends the turns. Otherwise
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
no longer open on a restart (`release_superseded`). This is a precondition of every
`setRotation(true)`, not only the first: `operatorInEffect().release` must equal `currentRelease()`.
A rotation-off upgrade moves only the vault's release, so it must also move the registry's, by
the candidacy's move to an approved `B` (section 5, steps 2 and 3) or, for an unregistered installed
operator, `installOperator` again with `B`'s release.

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
   (expected). The successor connects and your node hands over. It then follows the successor, as a
   member does between turns, and opens none of its keys again; a restart of it does the same.
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
4. **Fold the change into the vault's generation** (anyone may; `B` reads it at finality, waits up
   to a minute, then refuses, and `A` hands over no keys until it lands):

   ```sh
   cast send $VAULT 'syncRelease()' --rpc-url $RPC --account <any>
   ```

   Check, once the sync block is final: `cast call $VAULT 'syncedRelease()(bytes32)' --block finalized`
   equals `cast call $VAULT 'currentRelease()(bytes32)' --block finalized`, and `releaseGeneration()`
   at `--block finalized` rose by one. Do not start `B` on the latest block's reading: it checks at
   finality and refuses if the sync block is not yet final. `A` raises `release_superseded` (Critical) once the
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

## 6. Daily turns

With a schedule named, each election close while rotation is on adopts every eligible candidate as
the rotation set, weighted by delegated stake, seven days plus the finality margin before the set
takes effect; each close's set waits out its own notice, however many wait. The quorum counts only
the votes of the eligible candidates holding a promotable standby key, the ones the set adopts by
votes. A member of the latest set between standby keys at a close, as while it moves to a new
release, keeps its place for that close and any within an epoch length after it: register the new
release's standby key within an epoch, or the next close with the quorum leaves the member out. Each
key registered starts that epoch afresh for the next move. Without a quorum the latest set stands, less the
members no longer eligible. Each carried member is weighted by no more than its votes at the close.
Anyone draws turn `n`, which starts at 22:30 UTC on day `n` of the Unix epoch, from a day before that
boundary until it: `cast send $SCHEDULE 'draw(uint64)' $n`. Turns are
a day long: `cast call $SCHEDULE 'turnLength()(uint64)'` answers 86400, and a production node refuses
a schedule that answers anything else (`custody_chain_inconsistent`). Shorter turns exist only in
development builds (the `short-turns` feature), which a production build refuses to compile. Draw each
turn as soon as its deadline passes, so the engine mirrors it well before its boundary; a turn not
drawn by its boundary is not drawn at all, and the member before it stays named.

- **Deploy order.** Every member of the rotation set must run the release that follows and hands
  over only the tail before the timelock turns rotation on: every request for the keys now names the
  newest protocol version the successor's build reads, which an older incumbent does not read, so a
  member on this release takes no turn from it, tail or whole log. Upgrade each candidate
  (`setRelease`, section 5) first, then turn rotation on.
- **Before your turn.** Run the node with `operator.rotation_enabled = true`, `[log_heads]` (the node refuses to start
  without it), sealing and a handover listener, and `custody.evm.staking.registry.schedule` naming the schedule. `cast call $SCHEDULE
  'turnOf(uint64)' $n` shows the member, its generation and the turn before it, whose handover
  endpoint you take the keys from.
- **Between your turns.** Keep the node running, started with no key flag. While a turn names another
  member it follows the operator in effect: the candidate the registry names, at `https://` its
  registered API hostname, every frame verified against the vault's signer at the latest block. It keeps a
  copy of the log with a live replica built by one replay when it starts following, resolves the
  operator again at each boundary and after a failed stint, and pins the latest posted log head as
  its replica passes it. `standby_follow_failed`, `operator_log_rollback` and `replica_diverged`
  ([alerts.md](alerts.md)) page what it finds, each once per episode, and `member_follow_blocked`
  a step of following (reading the turn, resolving the operator, opening the copy, reading or
  pinning the posted head) that fails two polls in a row; each resolves once it works again.
- **At the boundary.** A following node whose turn is next stops following at the boundary by its
  trusted time, not at the end of a stint. It reads the vault, the schedule, the signers, the checkpoint
  anchor at the vault's finalized checkpoint (the incumbent's checkpoint still in flight is in its
  copy, and its node resumes it as outstanding), the MONKEY budget and the latest posted log head,
  and takes its trusted time sample, all before it connects; one short stint then catches its copy
  up, and it asks the incumbent for the keys and only the entries past its copy's tip, takes them onto its copy and replica, and starts sequencing from the replica with no replay
  of the log and no chain read or time sample in between, so the switch costs the incumbent's final
  checkpoint, the tail's transfer and the node's start. `engine_full_replays` stays put across it.
  With `[snapshots]` configured, a member that took the tail does not start from its replica: it
  recovers from disk, waits for checkpoint finality and may replay the whole log inside the switch,
  so leave `[snapshots]` unset on rotation-set members. An incumbent that does
  not find the tip on its log refuses before it stops, keeping the keys and sequencing on: Critical
  `handover_tip_not_on_log` for a copy that diverged, Warning `handover_tail_lagged` for one older
  than the 65,536 prefixes the incumbent retains or with a tail over its bound, which your node
  catches up and retries. After three refusals it sets its copy aside (`<log dir>.aside`, replacing
  any earlier one) and takes the whole log as a node with no copy does, since its copy's segment
  files are its own writer's, not the holder's; an election naming your node, or a turn whose copy
  does not open, takes the whole log the same way. Other failures of a take (the chain reads or the
  trusted time sample before it connects, or no source answering) are retried for five minutes
  before it does the same, so a passing RPC or Roughtime outage does not cost the tail. A copy that is stale when your turn comes, after a
  restart, follows the incumbent until it is within 4,096 commands of its head, for ten stints at
  most, before its first take. Once a successor confirms, the incumbent's
  node goes back to following, now its successor. A node started at the boundary with no copy takes
  a trusted time sample, reads the turn from the finalised schedule, connects to the incumbent's
  handover endpoint, and takes the keys and the log's tail; neither side waits for chain finality
  after the boundary. When the turn before never changed hands, because its member never took it, the node
  tries each earlier turn's latest member and source in turn, a week back at most, then the members
  those turns' skips passed, and the operator still holding the keys hands them over. That includes an incumbent that stopped for an earlier member whose transfer
  never confirmed (`handover_unconfirmed`): it admits you when the set your turn was drawn from names
  your candidate and key with seven days' notice by your boundary, from V53 counted from the first of
  the unbroken run of sets that named them. Failing that, start it
  with `--predecessor <host:port>` naming the holder's handover endpoint. Its first command is
  `TurnTaken`, which starts your time in office for the operator's share. A turn you hold again the
  next day needs no handover: your node logs `TurnTaken` for it at the boundary. Neither does a turn
  you are named for after one whose member never took it, while your node still holds the keys: the
  schedule gives that turn a new generation, and your node seals the keys in it before it logs
  `TurnTaken`, so a restart opens them.
- **After your turn.** Your key stops signing at the next boundary, and you earn nothing past it,
  even if your node keeps sequencing because no successor came. The next member's node takes the keys
  from yours at the boundary; `release_superseded` pages only if it has not 15 minutes on. Hand over
  as section 4 describes. Once a successor confirms, your node follows it and never takes back the
  generation it handed over, even when a later trusted time sample falls just before the boundary.
- **Withdrawals and the boundary's log check.** While rotation is on and a turn is in effect whose set
  names two or more members, the operator's node signs a withdrawal only once a check has verified the
  log through its approval, so an approval waits for the next boundary. At a handover the successor's
  start is the check: its replica verified the log through the checkpoint it was handed. Your own
  upgrade's start is no check, since your member sequenced the range: the approvals it holds wait for
  the next boundary's check, as they would have. At a boundary you hold on through, your node writes a checkpoint after its `TurnTaken`
  and asks the members of the turn's order, first to last, for an attested verification of it, in
  questions your enclave signs, skipping your own candidate, members with no handover endpoint, and
  members that sequenced part of the range since the last check under any release, or run a release
  that did; it asks again every minute, and `log_check_pending` pages after 30 minutes without one.
  Between your turns your node answers those questions on your registered handover endpoint, from the
  checkpoints its replica verified and only when the enclave that signed the checkpoint signed the
  question, so keep that endpoint reachable from every member's node, not only from the next turn's. A
  one-member set, or rotation off, needs no check. The verified head is sealed with the keys, and an
  upgrade (section 5) carries it to the new release; a start that finds its log does not hold the
  sealed head asks for the check again. Every member must run a release that has the log check, and
  names the releases that hand the keys on, before rotation is turned on, since keys carrying a
  verified head or those releases do not open on an older release; with
  rotation off the keys carry none ([deploy-and-rollback.md](deploy-and-rollback.md), "Know before you
  start"). Leave `[snapshots]` unset here too: a start from a snapshot
  past the verified head cannot say who sequenced the range and pages `log_check_unattributable`.
- **A turn the engine refused.** `turn_refused` ([alerts.md](alerts.md)) means no set mirrored at
  least seven days earlier names your candidate with your key: the key in effect stays for that turn.
  From V53 the seven days run from the first of the unbroken run of mirrored sets that named your
  candidate with that key, so a set mirrored late that names you unchanged keeps your notice, while
  a new key, or a return after a set that dropped you, waits seven days again.
- **A new protocol version.** A node activates its build's newest protocol version when it boots, and
  a build that predates a version cannot read a log that activated it. Move every candidate in the
  rotation sets to a release that implements the version before any member boots one, or the next
  member on an older release cannot take the keys and the log. An incumbent refuses, before it
  stops, a successor whose build reads only versions older than its log runs, and keeps the keys.
- **Rotation off.** Turning rotation off stops draws; the turns already drawn stand, and the last
  member stays named until the timelock installs an operator once they end.
- **Liveness.** A failed operator is replaced within minutes (section 6a); the registry's own `promote`
  reverts while a schedule is named. Once the turn's order is exhausted, the governance fallback
  applies: rotation off, then `installOperator` once the drawn turns end.

## 6a. Minute-scale liveness

While a turn names the operator, its enclave's log head is the heartbeat, and a member whose operator
misses it is promoted at once.

- **What the operator's node does.** It posts an `engine.log-head.v2` head at least twice per
  `heartbeatInterval` (the schedule's `liveness()`, 60 s by default) through `[log_heads]`'s relayer,
  which must hold enough for two posts a minute. With another member in the turn's set, it signs a
  head or a checkpoint only at a seq another member's enclave confirmed holding, so it sequences only
  while a member follows it: before it sequences after any start it asks the other members for their
  copies (`liveness_start_refused` names a member that holds more), and it stops taking commands while
  its log runs more than one interval past the newest confirmation (`liveness_unconfirmed`). Through
  a benchmark-sync hold or a trusted-time halt it posts held heads at the newest seq a member
  confirmed, so no promotion follows. It stops by
  its own clock 15 s before the earliest block time a promotion could pass (`liveness_fenced`), and
  resumes on the first read that moves that time on. A guardian retirement of the signer ends no run,
  but its heads stop landing, so while a turn names the operator it fences about `heartbeatMisses`
  intervals later, until the timelock installs a signer. `GET /v1/operator` names its `enclave` and
  the vault's `signer` epoch and key; a client takes an answer only while they match.
- **What every member's node does between its turns.** It follows the operator, syncing its copy every
  200 ms, and answers on its registered handover endpoint: its copy's status, signed by its standby key,
  the copy's latest entries, and vouches for other candidates' standby keys. With `[liveness]` it also
  witnesses each slot, skips once `skipDueAt()` passes, promotes once `promotionDueAt()` passes, and
  relays the operator's signed heads
  from its stream when the chain's head is older than two thirds of an interval. Fund
  `liveness.caller` apart from every relayer; `liveness_caller_unfunded` pages below
  `liveness.min_balance`.
  ```toml
  [liveness]
  caller = "0x..."        # an account the checkpoint endpoint signs for, apart from every relayer
  min_balance = "0.01"
  ```
- **Your standby key's certificate.** With `[api.tls.acme]`, your node keeps a public certificate for
  its standby key's TLS key while it follows: its API listener on `node.listen` answers the CA's
  TLS-ALPN-01 validation of your hostname then, so forward port 443 to it on a following member as on
  the operator. The certificate is sealed apart from your turn's (`*standby-certificates.json` in the
  sealing directory) and renewed as the API's is; `certificate_pending` and `certificate_expiring`
  page for it while you follow. A promotion serves it from its first answer, so browsers and the
  other members, which verify the operator against public roots, follow the promoted member at once.
- **Your standby key.** Your enclave seals a standby key for its release and asks the operator in
  effect's API (`POST /v1/standby/vouch`), then each member, to vouch for it. The schedule takes the
  registration only from your candidate account, which the node does not hold: once vouched, the node
  writes `standby-registration.json` in the sealing directory and pages `standby_unregistered` with the
  call. Send it before the vouch expires (30 minutes; the node asks again 10 minutes before):
  `cast send $SCHEDULE "$(jq -r .calldata standby-registration.json)"` from the candidate's account,
  or `cast send $SCHEDULE 'registerStandby(address,uint64,uint64,bytes)' <key> <nonce> <expiry> <vouch>`.
  `cast call $SCHEDULE 'promotable(address)' $CANDIDATE` shows the key once registered. Register before
  the timelock turns rotation on: start your member node with its rotation config while the switch is
  still off, and it prepares the key and asks the deployment's operator to vouch, then waits for the
  switch. A candidate
  with no promotable key at an election close is not adopted by its votes and its votes do not count
  toward the quorum; a member of the latest set keeps its place without one for about an epoch (see
  Daily turns). A `setRelease` voids the key, and the node registers a new one for the new release, so
  move releases early enough in an epoch for the new key to register before its close.
- **A promotion.** Once `heartbeatMisses` witnessed slots (5 by default) pass with no head, anyone's
  `promote()` names the next qualified member, and the vault moves its signer to that member's standby
  key in a new signer epoch. It prefers a member that held nothing of the turn, then one drawn or
  skipped to that never received the keys, and takes the key holder last; a member that held the keys
  under an assignment of the turn is never named again in it (`cast call $SCHEDULE
  'heldKeys(uint64,address)' <turn> <candidate>`). The promoted node first takes what any member holds past its copy, from the
  member that confirmed the head, then every other, then the outgoing operator's archive, and starts
  sequencing from its copy at once if the copy holds the outgoing epoch's latest head and the vault's
  latest checkpoint the promotion fixed; `promotion_start_refused` pages a copy that cannot, and the next
  promotion passes it. It records the promotion in `promotion.json` in the sealing directory; a restart
  resumes from the copy while the chain records the same promotion, and never cuts the copy back to the
  vault's anchor. Its checkpoints and payouts wait for the promotion's finality; `promotion_reorged`
  pages a promotion the latest block no longer records, which stops the node. Restart it: it waits,
  signing nothing, until a final block settles the promotion, resumes if the final block records it, and
  otherwise sets the copy, the resealed keys and the record aside (`*.quarantined-<turn>-<position>`
  beside each), pages `promotion_reorged` again, and follows as a member with a new standby key. Keep
  the set-aside files for the owner.
- **A turn its member does not take.** A member takes its turn with its first head under its
  assignment, which `LogHeads` latches; post it as soon as the node holds the keys. Once `takeWindow`
  witnessed slots (`cast call $SCHEDULE 'takeWindow()'`, 2 by default) pass after the latest of the
  boundary's slot, the assignment's and the last signer change's with no latch, anyone's `skip()` names
  the next listed member in the turn's order that has held no assignment of the turn, other than the
  key holder: the candidate whose assignment recorded the release that signed the newest head
  (`cast call $SCHEDULE 'releaseCandidate(bytes32)' $(cast call $LOG_HEADS 'headRelease(uint64)'
  <epoch>)`), whatever release it has moved to since, in a new generation,
  with no signer change; an unwitnessed slot, as through a chain outage, restarts the window, a
  promotion's assignment is never skipped, and no skip is due while the guardian has retired the signer. The timelock sets the window within its deployment bounds while no turn
  names the operator (`setTakeWindow`); the deployment's minimum must exceed the switch budget plus one
  interval plus a head's inclusion (`headTolerance`) at the shortest interval the bounds allow. The
  switch budget covers the daily rotation scenario's measured switch plus two reads of the turn's latest
  assignment after the stop, the holder's and the successor's, each thirteen round trips to the custody
  providers one after another, fourteen while a provider's tip is past the agreed block: at 100 ms a
  round trip, up to 2.8 s on top of the measured switch. The schedule refuses a window
  (`setTakeWindow`), or a heartbeat rule (`setLiveness`), that would leave the first skip's member no
  such take before the untaken-turn promotion: `(heartbeatMisses - takeWindow) * heartbeatInterval`
  must exceed the switch budget plus the larger of `headTolerance` and the 15 s before a promotion at
  which the member's node stops signing, so to lengthen the window past that, raise `heartbeatMisses`
  first. Skips
  act at sequencer confirmation: the member named takes the keys, and the key holder admits it, from the
  turn's assignment at the custody quorum's latest agreed block, not finality, and the holder's log must
  show the skip's set naming the member with seven days' notice; a skip's member upgrading to another
  approved release hands its keys on only once the log has mirrored the skip at finality. Whoever holds
  the keys hands them on:
  the incumbent, or a member that took them late and was then skipped, which starts, sequences and hands
  them to the member now named; a holder whose skip a reorg removed hands them to the member the
  schedule names again. A member is found where its heads are newest, then among the turn's other
  assignments, then at the turn's source and earlier holders; a member following moves to the holder
  when the operator the registry names fails a stint. A skip's member with no copy of the log, or one
  restarting, reads the skip at its start the same way, before it is final; a member that handed its
  drawn turn to a skip's member takes it back once a reorg removes the skip. A holder the registry no
  longer names, as one a later skip or a reorg moved past, does not restart as a holder, as an incumbent
  past its boundary does not: it follows; leave its sealed keys, as a later skip or the missed-head
  promotion moves the turn on. If no assignment of a turn is latched once `heartbeatMisses + 1` witnessed slots pass after
  the later of the boundary's slot and the last signer change, in a set of two or more members,
  `promote()` passes even while the incumbent posts heads, which bind its own earlier turn, naming the
  next qualified member and the incumbent, which holds the keys, last: its own standby key only when no
  other member qualifies; the incumbent stops by its own clock
  before that, as `promotionDueAt()` counts this path too. With no member qualifying, the promotion is
  refused after the incumbent has stopped: use the governance fallback (rotation off, then
  `installOperator`).
- **A replaced operator coming back.** The outgoing operator, restarted or still running, follows the
  promoted member. Its log may hold commands past the last head its signer epoch posted that no member
  received; once a final block shows that epoch ended, the node sets its log aside
  (`<log_dir>.superseded-<seq>`), pages `superseded_tail_set_aside` with the commands it dropped, and
  rebuilds its copy from the promoted member's archive. Keep the set-aside log for the owner.
- **Pinned assumptions.** Arbitrum's block timestamps lead true time by no more than 10 s (the stop
  margin's first part) and the enclave's clock runs no more than 500 ppm slow. A sequencer outage
  produces no blocks, so no slot is marked and no promotion passes; the operator stops about
  `heartbeatMisses` intervals after its last read and resumes on its first read after the outage. The
  delayed inbox and forced inclusion take hours, so nothing in the rule relies on them. `headBlocks`
  must span less than one interval at the block rate the deployment pins (200 blocks at 250 ms).

## 7. Parameter votes

From protocol V56, with `[custody.evm.staking.registry.vote]` naming the `ParameterVote` contract over
the schedule, the members of the rotation set in effect vote on parameter changes while rotation is
on. Votes govern while the registry's `rotationEnabled()` holds and, from V65, a rotation set with
members is in effect (`cast call $SCHEDULE 'currentSetId()(uint32)'` prints a nonzero id, and
`setOf` that id lists candidates), which is when members can propose: from rotation's start until the
first set takes effect, seven days plus the finality margin after the close that adopts it, and while
a set with no members is in effect, the operator signs the governed commands within its own bounds,
as with rotation off. A drawn turn keeps `rotationEnabled()` true from its boundary after the switch
turns off, until an installation; the node mirrors the registry's named turn with each finalised
staking frontier. While votes govern, your key, as the turn's operator,
keeps benchmark rates, its own stress floor (level 1 at most, lapsing after 72 hours), reduce-only,
and before V76 corporate actions; `SetParams`, `UpsertMarket`, the parameter, markup and listing
notices and close-spread adoptions it signs are refused, from V76 so are corporate actions
([corporate-actions.md](corporate-actions.md)), and so are the node's own protocol activation and
access policy, which wait for a vote. A relisting a runbook asks for (a futures contract, a price rule,
a calendar) or a corporate action is then a normal vote: propose it at least seven days, plus the
window it takes to pass, before it must apply, and a corporate action two trading days more.

- **Before proposing a command a release added** (a protocol activation, a new field), every member
  moves to a release that knows it: a node whose release cannot decode a proposal's bytes mirrors it as
  no command, and the engine refuses it.
- **Before proposing an access policy**, every member moves to a release whose config's `[access]`
  section names exactly that policy: its `source`, and a `max_age` equal to its `max_age_ms`, as for a
  command a release added. Once the vote applies the log records
  it, and a node whose config names no policy or another refuses to start on that log ("the configured
  access policy differs from the public log"), so a member on an older release cannot take its turn
  and the incumbent holds on until that member's release is approved and deployed.
- **Propose.** A member proposes one admin command as the compact JSON the engine writes for it (the
  `command` of the request `engine admin` prints: no spaces, fields in the engine's order, defaults
  left out), since the engine refuses any other encoding of the same command:
  `cast send $VOTE 'propose(bytes,bool)' 0x<hex of the JSON> <fast> --from <candidate>`. The proposal
  casts the proposer's yes vote. A `SetParams` or `UpsertMarket` names every field: take the values in
  force, with the baseline from `GET /v1/house/state`'s `governance` section where a restriction holds
  one, and change only what the vote is for. `GET /v1/operators` shows `command: null` for a proposal
  whose bytes are no such encoding.
- **Vote.** Each member of the set the proposal opened under votes with `cast send $VOTE 'vote(uint256)'
  <id> --from <candidate>`, until the window closes (`normalWindow()` or `fastWindow()`). It passes when
  the yes weight is more than two thirds of the set's; `GET /v1/operators` lists the open proposals with
  their weights.
- **A normal change** needs no notice of its own: its seven days are the vote's. It meets every other
  bound the operator's command would: a relisting keeps each changed spread band from its launch rate
  to twice it and each changed margin requirement at or above launch's, new parameters keep every size
  multiplier at 1x or more, each toxicity score tier within ±50% of its launch score and add-on, the
  profit fee base at or above its launch value and the fee curve at or above its launch curve,
  a relisting keeps its market's markout floor, and a ceiling covers its market's launch close
  charge. It takes effect
  seven days after the engine logs it: `GET /v1/house/state` lists it
  under `governance.pending` with `applies_at`, and the node applies it at its first command from then.
  It applies only over the baseline it passed on: a `SetParams` or `UpsertMarket` whose parameters or
  market changed in the meantime, by another vote or, while votes do not govern, the operator, is refused rather
  than undo that change, and so is a ceiling adoption while another waits. A change that applied while
  a proposal was still open is not caught that way: the proposal carries every field, so check it
  against the values in force before voting for it. `proposal_rejected`
  ([alerts.md](alerts.md)) means the command failed its checks then; propose it again over the
  parameters in force.
- **The fast path** takes only a higher margin band or close-out level, a stress floor at any level, a
  lower house or market cap, or reduce-only, with every other field as the baseline sets it. It applies
  at once as a restriction listed under `governance.restrictions` with its `expires_at`; a fast-path
  vote on the same command, or on one holding the same bounds, renews it, and nothing lifts it early:
  new parameters that drop a stress level a restriction holds a floor at are refused. A margin raise
  has no range beyond each band's validation and closes out the positions it leaves short at once. `proposal_refused` means the engine
  refused the proposal for the reason in the message.

## 8. Operator faults

From protocol V70, a member whose enclave signs two histories during its turn, as one its host rolls
back over the unposted window does, can be proven at fault. Anyone holding two of its
`engine.log-head.v2` heads of one signer epoch that bind the same turn, position and release and name
different links at one seq (the signed stream carries every head, and followers relay them) calls
`FaultProof.prove(epoch, a, b)` with each head as `LogHeads.postV2` takes it, its signature and the
index of the assignment's revision it binds (`cast call $SCHEDULE 'revisionOf(uint64,uint16,uint256)'`
reads one), from any funded account;
`FaultProven` names the member once per assignment. The engine then cuts that member's share of the
operator's earnings by a haircut that starts at the recorded penalty and falls linearly to nothing over
its decay, the withheld part going to stakers; `GET /v1/house/state`'s `operator_faults` section shows
the penalty and when each haircut started, and the member's history lists each
`operator_share_withheld`.

- **Your config** names the same `[custody.evm.staking.registry.fault_proof]` contract and penalty as
  every member's: the log records them once V70 is in force, and a node whose config names none or
  others refuses to start on that log ("the log records fault proof contract"), so your turn would not
  start.
- **A fault proven before V70 is in force** is credited and cuts nothing (`operator_fault_refused`).
- **Never restart your enclave from an older copy of its log during a turn**: a rollback within the
  unposted window is the fault this proves, whatever the host meant by it.

## Open questions

These are gaps in the contracts as written. Do not work around them on a live system; the owner
decides.

- **The first election needs a registered incumbent** (section 2). The contracts accept an election
  that leaves `handoverSource()` empty, and the successor then cannot start.
