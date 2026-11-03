# Runbooks

Procedures for running the engine node in production. Each one is written to be followed in order
by an operator who has not read the code: numbered steps, and under each step what to check before
going on. Start with [reference.md](reference.md), which sets up the variables and the checks the
other runbooks call.

| To do this | Read |
| --- | --- |
| Install a release, upgrade to one, or go back to the last | [deploy-and-rollback.md](deploy-and-rollback.md) |
| Stop new risk in one market or all of them | [reduce-only.md](reduce-only.md) |
| Bring a stopped or crashed node back | [restart-and-recovery.md](restart-and-recovery.md) |
| Lose, replace or rotate a key | [key-recovery.md](key-recovery.md) |
| Schedule a dividend or a split | [corporate-actions.md](corporate-actions.md) |
| Respond to a page you do not recognise, or to anything that looks like an attack | [incident-response.md](incident-response.md) |
| Withdrawals stop being authorised, or `checkpoint_backlog` fires | [withdrawal-stalls.md](withdrawal-stalls.md) |
| A price feed fails (a crypto venue outage halts BTC-USD and ETH-USD), a custody RPC provider fails, or the checkpoint endpoint fails | [feed-and-custody-failures.md](feed-and-custody-failures.md) |
| The API certificate is missing or will not renew | [certificate-renewal-failure.md](certificate-renewal-failure.md) |
| `unattested_certificate` fires | [unattested-certificate.md](unattested-certificate.md) |
| Stand for election as the operator, or hand the node to or from the operator in effect | [operator-candidate.md](operator-candidate.md) |
| Run a standby, take over after a promotion, or recover when none qualifies | [standby.md](standby.md) |
| Find what an alert means and which runbook answers it | [alerts.md](alerts.md) |
| Write the node's config | [production-node.toml](production-node.toml) |

## How the steps are marked

- **Changes the live system.** A step that restarts or reconfigures a node, sends an admin command,
  calls a contract, moves house capital, or writes or hands over a key says so and names the approval
  it needs. A step without that line only reads.
- **One approval covers one action.** The repository's [no-deploy rule](../../agents/rules/no-deploy.md)
  applies to every such step: an approval to deploy is not one to change the config, and one for
  a testnet is not one for mainnet. If you are unsure whether you have approval, you do not: write
  the exact command and ask.
- **Who approves.** The deployment owner approves node, config and admin-command changes. The
  contracts' timelock approves its calls: the deployment's multisig schedules them, and anyone
  executes them once the delay has passed. The guardian is the same multisig acting without the
  delay, and can only pause withdrawals for seven days or retire the signer.
- **Commands run on an admin workstation**, not on the node's host, unless a step says otherwise.
  Nothing in these runbooks asks you to log in to the enclave: it has no shell.

## What is not in place yet

These runbooks describe the system as the code builds it today. These parts are not done, and a
runbook that depends on one says so where it does:

- **The image has not run on SEV-SNP hardware** (#108).
  The quote, the launch measurement, sealing and a delivery of secrets on a real host are unverified.
- **No staging environment exists.** The deploy and rollback procedures have been checked against
  the pieces that run locally, not end to end.
- **The values marked `OWNER` in [production-node.toml](production-node.toml)** wait on decisions:
  the vault's and staking contract's launch values (#436),
  the vendor accounts and the access source.

## Keeping these in step

An alert the node raises, a command it takes, or a config key it reads that is missing from these
pages is a gap in them. [alerts.md](alerts.md) lists every alert the code raises; add a row there
in the same change that adds an alert.
