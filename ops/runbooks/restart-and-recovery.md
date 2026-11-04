# Restart and recovery

The node's state is its log. Every command is on the disk before a client is told it applied, so a
stopped or crashed node loses no acknowledged command. A restart replays the log, checks it against
the vault, opens the sealed keys, and carries on. This runbook restarts a node, reads a node that
refuses to start, and recovers from a lost disk.

Read [reference.md](reference.md) first.

## Facts that shape every step

- **Every stop of the image is a hard stop.** The guest ignores an ACPI shutdown request, so the
  engine does not drain its queue or write its final checkpoint. Commands after the last checkpoint
  stay in the log and are covered by the next checkpoint the restarted node signs. (A node run by
  hand, outside the image, drains and writes a final checkpoint on SIGTERM or ctrl-c.)
- **Where the logs are.** The engine writes its log lines to the guest's serial console. Capture it
  to a file on the host and keep it; every `alert =` line below is read there, and the webhook
  receives the same alerts as pages.
- **Restart keeps the keys.** With `[tee.sealing]`, the same image and config on the same chip open
  the same sealed keys: the enclave address, signer epoch, TLS key (so `$PIN`), certificate and
  outstanding withdrawal authorizations are unchanged. A restart needs no governance action.
- **A restart can wait on the chain.** If a posted checkpoint has not yet become `finalized`, startup
  waits for it, for up to `custody.evm.finality_stalled_after` (30 minutes in the template), before
  it opens the log. After a restart that follows a recent post, expect up to that long before the API
  answers.
- **Time must be trusted first.** Nothing is sequenced until the first Roughtime sample arrives, and
  sequencing halts if the host clock drifts. See `clock_drift` in [alerts.md](alerts.md).
- **Deadlines run while the node is down** (reference.md, "Deadlines that run while the node is
  down"). Note the time of the last checkpoint before you begin.

## Restart a healthy node

1. Note `cast call $VAULT 'lastCheckpointAt()(uint64)' --rpc-url $RPC` and
   `cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC`.
2. If new risk during the restart worries you, make the markets reduce-only first
   ([reduce-only.md](reduce-only.md)).
3. Stop the VM, then start it again with the same data disk and no marker files. No `new-key`, no
   `predecessor`. **Changes the live system.** Needs the owner's approval to restart this node.
4. Watch the console. In order, a healthy start logs: the enclave, config and TLS key attested;
   history index opened; `recovered from log` with the command and checkpoint counts; the Roughtime
   servers; `metrics up`; `api up over TLS`; `first trusted time sample: sequencing started`.
5. Check:
   - `curl -sf $API/v1/health` prints `ok`, and `curl -sf $API/v1/attestation | jq -r .enclave_address` equals `$SIGNER`.
   - The pin is unchanged: `jq -r '.tls_public_key_sha256[2:]'` of the new attestation equals `$PIN`.
   - `cast call $VAULT 'latestSeq()(uint64)'` rises again within a few minutes.
   - `engine_withdrawals_paused` returns to `0` after the first reconciliation. It reads `1` until then.
   - No page is open that was not open before. Pages from the stop (`EngineNodeDown`) resolve.
6. Restart the companions that depend on the node: the benchmark worker ([deploy-and-rollback.md](deploy-and-rollback.md),
   section 5). A node with `[benchmarks]` sequences nothing but required commands until the worker
   has delivered a snapshot (`benchmark_sync_pending`).
7. Tell clients that a signed withdrawal reissue request queued before the restart was lost: the
   account sends the same signed body again (the body stays valid until a new authorization replaces it).

## The node will not start

A start that fails prints `refusing to serve: <cause>` on the console under one of the alerts
below, and the image then powers the VM off. Read the cause after the colon; the table says what
each alert covers and what to do. **Do not delete log files, the checkpoint sidecar, or sealed keys
to make a refusal go away**: each refusal is a check that something is wrong, and the log is the
only record of the books.

| Alert | What it checks | First action |
| --- | --- | --- |
| `secret_missing`, `secrets_unreadable` | Not refusals: the node lacks a delivered secret, or its sealed file does not open. It holds for a chain or screening key, serving only its delivery listener. | Deliver ([key-recovery.md](key-recovery.md), "Secrets"); it goes on starting. |
| `secrets_start_failed` | The first read of the chain failed on a delivered key. The VM powers off after five minutes, or once a new bundle is sealed. | [alerts.md](alerts.md). |
| `clocksource_unprotected` | The guest kernel times on the TSC, which Secure TSC protects, in the frequency the hardware reports. The cause names the clock it times on instead: the host made the kernel mark the TSC unstable at boot, for example by naming another hypervisor in its CPUID leaves. Or it names a frequency under 1000 MHz: the host asked at launch for one the hardware cannot count in, and the kernel calibrated the TSC against clocks the host emulates. | For another clock, the kernel's `Marking TSC unstable due to <reason>` line earlier on the console says why. Start the VM as `image/README.md`, "Boot on an SEV-SNP host", says, with no other hypervisor's CPUID leaves. For a low frequency, the host's launch request is wrong: relaunch asking for the processor's TSC frequency. Never run the node on another clock. |
| `enclave_key_failed` | The sealed keys cannot be opened or made: none sealed for this release and generation, a changed config, another chip, a lower firmware, or `new-key` beside an existing file. | [key-recovery.md](key-recovery.md). |
| `signer_unregistered` | Not a refusal: the vault does not name this key, so the node serves only its attestation. | Governance registers the key ([key-recovery.md](key-recovery.md), "New keys"). |
| `custody_connect_failed` | The custody adapter could not start: the providers could not be reached, or too few agreed on the chain and token. | [feed-and-custody-failures.md](feed-and-custody-failures.md). |
| `checkpoint_startup_failed` | The log against the vault, before anything is written. The cause says which: `vault signer differs from this enclave; signer recovery is required`, `log is shorter than the latest posted checkpoint`, `log differs from the latest posted checkpoint at seq N`, `vault did not authenticate the posted checkpoint`, a checkpoint RPC that cannot be read, or the checkpoint sidecar naming work the log does not hold. | See "A log shorter than the vault" below. For an unreadable RPC, [feed-and-custody-failures.md](feed-and-custody-failures.md). |
| `log_recovery_failed` | The log against the config and the rules: a config naming another operator, client auth, access policy, chain, vault or genesis than the log records; a signature or root that does not verify; a command after the last checkpoint that the enclave key did not seal. | Compare the config with the log's banner (below). A config that differs from what the log records is a wrong config: use the one the log was written under. Never edit the log. |
| `history_recovery_failed`, `marks_cache_failed` | The account history index or the marks cache cannot be opened or repaired. Both are derived from the log. | See "Rebuild the derived stores" below. |
| `emission_budget_mismatch` | The log records an emission schedule whose period budget differs from the one the vault's MONKEY cap was derived from. | Stop and call the owner. This cannot be repaired on a live log. |
| `genesis_refused`, `protocol_activation_failed` | The config's genesis, or the newest protocol version, does not apply to this log. | The config is wrong for this log, or the build is older than the log. Use the release that wrote the log; see [deploy-and-rollback.md](deploy-and-rollback.md), "Know before you start". |
| `benchmark_sync_prerequisites_missing` | A log with a rollover schedule lacks client auth, the operator or toxicity weighting. | A log from before those were recorded. Not expected on a production log; call the owner. |
| `collateral_relay_failed` | The vendor collateral relay could not be set up. With SEV-SNP the usual cause is a missing `tee.processor`. | Fix the config: a new release. |
| `legacy_withdrawal_unproven` | An inherited withdrawal has no canonical finality proof. | Not expected on a production log; call the owner. |

The log's banner states what it records. From a downloaded archive:

```sh
$ENGINE replay --log archive
# verified: <n> commands, <n> checkpoints, final root 0x…, protocol V…, client auth …, operator 0x… (chain …, vault 0x…)
# checkpoint signers: 0x…
```

Compare the operator, chain and vault to the config's `operator.public_key`'s address,
`custody.chain_id` and `custody.vault`.

### A log shorter than the vault

`log is shorter than the latest posted checkpoint` means the log on this disk ends before a
checkpoint the vault already accepted, or `log differs from the latest posted checkpoint` says the
bytes differ. Either the disk was rolled back or replaced, or the host altered it. The node refuses
because signing on this log would diverge from the vault.

1. Do not start it again. Keep the disk as it is.
2. Find a copy that reaches the vault's `latestSeq`: a standby's copy ([standby.md](standby.md)), then
   the archive mirrors. A mirror holds completed segments only; the tail past its last segment is not
   in it.
3. Check the candidate: `$ENGINE replay --log <candidate>` ends in `verified:`, and its last
   checkpoint seq is at least `cast call $VAULT 'latestSeq()(uint64)'`, with the same root as
   `latestRoot()`.
4. Put it on a fresh data disk with the config. Start per "Restart a healthy node".
   **Changes the live system.** Needs the owner's approval to replace the log.
5. If no copy reaches the vault's checkpoint, the books past the last mirrored checkpoint cannot be
   rebuilt. Stop. This is the case the exit exists for: see [incident-response.md](incident-response.md),
   "Checkpoints cannot resume".

### Rebuild the derived stores

The account history index and the marks cache are rebuilt from the log when their directory is gone.
With the VM stopped and the data disk mounted on the host, remove `history/` (and nothing else) and
start the VM. The rebuild is a replay: allow time proportional to the log. Check the console reaches
`history index holds the log's tip`. **Changes the live system.** Needs the owner's approval to
restart this node.

## A lost or damaged data disk

| What is lost | Effect | Recover |
| --- | --- | --- |
| `history/` only | Nothing is lost; the index rebuilds. | Above. |
| `alerts/` | Pages not yet delivered are lost. | Start; the outbox is created empty. Check `engine_alert_pages_pending`. |
| The cursors | The deposit and staking scans restart from `custody.evm.start_block` and credit nothing twice. | Start; the scan is slow to catch up (`custody_deposits_lagging`). |
| The log | The books. | A copy that reaches the vault's `latestSeq`, as above. |
| The sealed keys, or the chip | The signing key. | [key-recovery.md](key-recovery.md), "New keys". |
| `config.toml` | The release's config. | The published `node.toml` of the release the vault names. Check `sha256sum` against the vault's `currentRelease()`: they must match or the keys will not open. |

## After any restart

- Write down what stopped, when, and how long the checkpoints were stale (`lastCheckpointAt()` before
  and after). The owner needs it if a deadline was close.
- If the stop was not planned, find the cause in the console log before closing it:
  `sequencer_failed`, `task_stopped` or `sequencer_stalled` mean a fault in the node;
  [incident-response.md](incident-response.md) covers reporting one.
