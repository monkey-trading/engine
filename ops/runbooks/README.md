# Runbooks

The operator's procedures for running the trading server in production.

Each runbook is written to be followed in order by an operator who has not read the code. It gives
numbered steps, and under each step what to check before going on. Start with
[reference.md](reference.md), which sets up the variables and the checks that the other runbooks
use.

| To do this | Read |
| --- | --- |
| Set up the shared variables and checks | [reference.md](reference.md) |
| Install a release, upgrade to one, or return to the previous one | [deploy-and-rollback.md](deploy-and-rollback.md) |
| Deploy the contracts, and initialise emissions and staking | [deploy-and-rollback.md](deploy-and-rollback.md#deploy-the-contracts), with the values in [production-deployment.toml](production-deployment.toml) |
| Write the server's configuration | [production-node.toml](production-node.toml) |
| Stop new positions in one market or in all of them | [reduce-only.md](reduce-only.md) |
| Bring a stopped or crashed server back | [restart-and-recovery.md](restart-and-recovery.md) |
| Replace, rotate or recover a key, or deliver the server's secrets | [key-recovery.md](key-recovery.md) |
| Schedule a dividend or a split | [corporate-actions.md](corporate-actions.md) |
| Respond to an unfamiliar alert, or to anything that looks like an attack | [incident-response.md](incident-response.md) |
| Act when withdrawals stop being authorised, or when `checkpoint_backlog` fires | [withdrawal-stalls.md](withdrawal-stalls.md) |
| Act when a price feed, a custody RPC provider or the checkpoint endpoint fails | [feed-and-custody-failures.md](feed-and-custody-failures.md) |
| Tighten a market's `max_age_ms` | [feed-and-custody-failures.md](feed-and-custody-failures.md#tightening-max_age_ms) |
| Act when the API certificate is missing or does not renew | [certificate-renewal-failure.md](certificate-renewal-failure.md) |
| Act when `unattested_certificate` fires | [unattested-certificate.md](unattested-certificate.md) |
| Stand for election as the operator, take daily turns, or hand the server to or from the operator in effect | [operator-candidate.md](operator-candidate.md) |
| Run a standby, take over after a promotion, or recover when no standby qualifies | [standby.md](standby.md) |
| Run the seed relayer, or act when one of its alerts fires | [seed-relay.md](seed-relay.md) |
| Find what an alert means and which runbook answers it | [alerts.md](alerts.md) |

## How the steps are marked

- **Changes the live system.** A step that restarts or reconfigures a server, sends an
  administrative instruction, calls a contract, moves house capital, or writes or hands over a key
  says so, and names the approval it needs. A step without that line only reads.
- **One approval covers one action.** An approval to deploy is not an approval to change the
  configuration, and an approval for a test network is not one for mainnet. If you are unsure
  whether you have approval, you do not have it: write down the exact command and ask.
- **Who approves.** The deployment's owner approves changes to the server, its configuration and
  administrative instructions. The contracts' timelock approves its own calls: the deployment's
  multisig schedules them, and anyone can execute them once the delay has passed. The guardian is
  the same multisig acting without the delay, and it can only pause withdrawals for seven days or
  retire the signing key.
- **Commands run on an administrative workstation**, not on the server's host, unless a step says
  otherwise. No step asks you to log in to the enclave, which has no shell.

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
