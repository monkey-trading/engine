# Runbooks

The operator's procedures for running the trading server in production. Commands, configuration keys
and output call a running trading server a node.

Each runbook is written for an operator who has not read the code, and who may be in the middle of
an incident. Each procedure starts from the alert, symptom or decision that brings you to it, and
says what is happening and how it affects clients. Numbered steps follow, one action each, with the
exact command and the output that shows it worked. The procedure ends with how to tell that the
problem is resolved, and when to escalate. Start with [Reference](reference.md), which sets up the
variables and the checks that the other runbooks use.

| To do this | Read |
| --- | --- |
| Set up the shared variables and checks | [Reference](reference.md) |
| Find what an alert means and which runbook answers it | [Alerts](alerts.md) |
| Install a release, upgrade to one, or return to the previous one | [Deploy and rollback](deploy-and-rollback.md) |
| Deploy the contracts, and initialise emissions and staking | [Deploy the contracts](deploy-and-rollback.md#deploy-the-contracts), with the values in [production-deployment.toml](production-deployment.toml) |
| Deploy the contracts that daily operator turns need | [Deploy the rotation contracts](deploy-and-rollback.md#deploy-the-rotation-contracts), with the values in [production-deployment.toml](production-deployment.toml) |
| Write the trading server's configuration | [production-node.toml](production-node.toml) |
| Stop new positions in one market or in all of them | [Make markets reduce-only](reduce-only.md) |
| Bring a stopped or crashed trading server back | [Restart and recovery](restart-and-recovery.md) |
| Act when `daily_snapshot_failed` or `daily_snapshot_unreadable` fires | [Daily snapshots](restart-and-recovery.md#daily-snapshots) |
| Replace, rotate or recover a key, or deliver the trading server's secrets | [Key recovery](key-recovery.md) |
| Schedule a dividend or a split | [Corporate actions](corporate-actions.md) |
| Respond to an unfamiliar alert, or to anything that looks like an attack | [Incident response](incident-response.md) |
| Act when withdrawals stop being authorised, or when `checkpoint_backlog` fires | [Withdrawal stalls](withdrawal-stalls.md) |
| Act when a price feed, a custody RPC provider or the checkpoint endpoint fails | [Feed and custody failures](feed-and-custody-failures.md) |
| Tighten a market's `max_age_ms` | [Tightening `max_age_ms`](feed-and-custody-failures.md#tightening-max_age_ms) |
| Act when the API certificate is missing or does not renew | [Certificate renewal failure](certificate-renewal-failure.md) |
| Act when `unattested_certificate` fires | [`unattested_certificate`](unattested-certificate.md) |
| Stand for election as the operator, take daily turns, or hand the trading server to or from the operator in effect | [Standing as an operator candidate](operator-candidate.md) |
| Run a standby, take over after a promotion, or recover when no standby qualifies | [Running as a standby](standby.md) |
| Run the seed relayer, or act when one of its alerts fires | [Seed relay](seed-relay.md) |

## How the steps are marked

- **Changes the live system.** A step that restarts or reconfigures a trading server, sends an
  administrative instruction, calls a contract, moves house capital, or writes or hands over a key
  says so, and names the approval that it needs. A step without that mark only reads.
- **One approval covers one action.** An approval to deploy is not an approval to change the
  configuration, and an approval for a test network is not one for mainnet. If you are unsure
  whether you have approval, you do not have it. Write down the exact command, and ask.
- **Who approves.** The deployment's owner approves changes to the trading server, to its
  configuration and to administrative instructions. The contracts' timelock approves its own calls.
  The deployment's multisig schedules each timelock call, and anyone can execute the call once the
  delay has passed. The guardian is the same multisig acting without the delay, and it can only
  pause withdrawals for seven days or retire the signing key.
- **Commands run on an administrative workstation,** not on the trading server's host, unless a step
  says otherwise. No step asks you to log in to the enclave, which has no shell.
- **Warnings** are in bold, at the step that they apply to, and say what goes wrong.

## What is not yet in place

The runbooks describe the system as the code builds it today. A runbook that depends on one of the
following parts says so where it does:

- **The image has not yet run on SEV-SNP hardware.** The attestation report, the launch measurement,
  key sealing and the delivery of secrets on a real host are not yet verified.
- **No staging environment exists.** The deployment and rollback procedures have been checked
  against the parts that run locally, but not end to end.
- **The values marked `OWNER` in [production-node.toml](production-node.toml)** wait on the vendor
  accounts and the screening source. The contracts' launch values are set in
  [production-deployment.toml](production-deployment.toml), and only the opening day is still to be
  confirmed.
