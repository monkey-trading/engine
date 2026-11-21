# Contracts

The Solidity contracts that hold the collateral, mint MONKEY, pay stakers and choose the operator,
as a Foundry project.

The vault on Arbitrum One holds every account's collateral as USDC. The engine keeps the trading
balances and the withdrawal queue. The vault accepts deposits, stores the checkpoints that the
enclave signs, and pays each withdrawal that the enclave approves, once the approval is proven
against an accepted checkpoint. If no checkpoint is posted for the exit period, anyone can start
the vault's exit, which pays each account what the last checkpoint shows it is owed. The vault is
not upgradeable. None of the contracts has been deployed to a public network yet.

## Contracts

| Contract | Where it runs | Purpose |
| --- | --- | --- |
| `Vault` | Arbitrum | Holds the USDC collateral. It takes deposits of 10 to 1,000,000,000 USDC, records the enclave's signed checkpoints, pays withdrawals and MONKEY and USDC claims proven against them, and runs the exit if checkpoints stop. |
| `ExitVerifier` | Arbitrum | The vault's proof checker for the exit. It values accounts at the last root and matches deposit receipts. Anyone can preview what an account's equity and credits register with `previewClaim`, which registers nothing, so a wallet can check its proof before the exit and during it. |
| `StateTree` | A library of `ExitVerifier`, which checks the vault's proofs | Checks a proof against the engine's state root, a sparse Merkle tree hashed with keccak-256 |
| `MonkeyToken` | Arbitrum | The capped MONKEY ERC-20 token, which only the vault mints |
| `Staking` | Arbitrum | Holds staked MONKEY, lets stakers delegate votes to operator candidates, enforces the unstaking cooldown and locks candidates' self-bonds |
| `OperatorRegistry` | Arbitrum | Registers operator candidates, chooses the rotation set at each close, and names the operator by election, by promotion or by daily turns. It deploys `Staking`. |
| `RegistryLens` | Arbitrum | A read-only, paged view of the registry, which the server reads for `/v1/operators` |
| `OperatorSchedule` | Arbitrum | Records the rotation sets, draws daily turns from them by stake, and runs the liveness rule, skips, promotions and standby keys |
| `SeedSchedule` | The base of `SeedRelay`, `SeedReceiver` and `OperatorSchedule` | The shared timetable of turn boundaries, draw deadlines and seed epochs |
| `SeedRelay` | Ethereum | Proves an epoch's RANDAO value from a beacon block root and sends it to Arbitrum |
| `SeedReceiver` | Arbitrum | Records the relayed seeds and answers each turn's seed. When no seed arrived, the fallback folds in the hash of an Arbitrum block recorded from the turn's draw deadline. |
| `LogHeads` | Arbitrum | Records the newest log head the enclave has signed, so that the host cannot roll back or fork the log unnoticed. While turns run, the same posts are the operator's heartbeats. |
| `ParameterVote` | Arbitrum | Stake-weighted votes of the rotation set on changes to the trading parameters. A proposal passes with more than two thirds of the votes. |
| `FaultProof` | Arbitrum | Proves that an operator's enclave signed two different histories at the same position in the log, and names the member responsible |
| `ArbSys` | An interface | The interface to Arbitrum's system contract at address `0x64` |

Each contract's NatSpec comments document its rules, its events and the reasons it reverts.

While operator rotation is on, each close of a staking epoch adopts a rotation set, which takes
effect seven days and a finality margin later. The quorum for a close counts only the votes of the
eligible candidates that hold a promotable standby key. With a quorum, the set holds those
candidates weighted by their votes. A member of the previous set that is between standby keys, as
it is while it moves to a new release, is carried into the new set for about one epoch, at no more
than its current votes. Without a quorum, the close keeps the previous set, less the members that
are no longer eligible. The schedule draws each daily turn from the set in effect, using the seed
relayed from Ethereum for that turn. When no seed was relayed, the draw takes two calls a block
apart: the first records the block whose hash the fallback seed folds in, and the second draws.
[The security model](../docs/security-model.md#the-seed-relay) states what Arbitrum's sequencer can do to a fallback draw.

A timelock with a nonzero delay governs the vault and the registry, with the deployment's multisig
as its proposer. The timelock rotates the signing key, approves releases, pauses deposits or
withdrawals, and switches operator rotation on and off. While rotation is off, it can install an
operator. While the registry's `rotationEnabled()` is false, only the timelock changes the vault's
release. A release belongs to the first candidate that the timelock approves it for or installs on
it. The deployment's release belongs to the deployment's operator. The registry refuses a release
for any candidate but its own. The timelock also moves the registry's parameters within the bounds
set at deployment. A guardian, which is the same multisig acting without the delay, can only pause
withdrawals for seven days and retire the signing key until the timelock installs another.

## Building and testing

You need Foundry 1.8.3. Run the following from `contracts/`:

```shell
forge soldeer install
forge fmt --check
forge build
forge test
```

`forge soldeer install` installs the dependencies pinned in `soldeer.lock`, forge-std 1.17.0 and
OpenZeppelin Contracts 5.7.0, into the ignored `dependencies/` directory. It needs to run only
once. `forge test` runs the unit, fuzz and invariant tests in about a minute and a half of CPU
time. It ends with a line of the form
`Ran <n> test suites in <time>: <n> tests passed, 0 failed, 0 skipped`.

[`foundry.toml`](foundry.toml) pins solc 0.8.36 and the `cancun` EVM version, which the vault's
transient-storage reentrancy guard needs. It also turns on the optimiser, without which the vault
exceeds the 24,576-byte contract size limit. The tests may read only `test/fixtures/` and the
engine's `crates/core/tests/fixtures/`, which hold the withdrawal, entitlement and exit proofs that
the engine and the contracts must agree on.

## Layout

| Path | Contents |
| --- | --- |
| `src/` | The contracts in the table above |
| `test/` | A test file per contract or area, with unit, fuzz and invariant tests |
| `test/helpers/` | Shared test bases and a builder for state trees |
| `test/mocks/` | Stand-in tokens, a contract wallet, an inbox and an `ArbSys` for the tests |
| `test/fixtures/` | The withdrawal vector shared with the engine, and recorded mainnet data for the seed relay |

## Deployment

- [Deploy and rollback](../ops/runbooks/deploy-and-rollback.md#deploy-the-contracts) gives the order
  in which the timelock and the vault are deployed, and the checks the vault's constructor applies.
- [`production-deployment.toml`](../ops/runbooks/production-deployment.toml) holds the launch
  values: the timelock's delay, the emission budget, the exit period, the claim window, the
  staleness limit, and the staking epoch and cooldown.
- [Seed relay](../ops/runbooks/seed-relay.md#deploy-the-relay-and-receiver) gives the order in which
  the seed receiver, the relay and the schedule are deployed.
- [Standing as an operator candidate](../ops/runbooks/operator-candidate.md) covers registering
  with the registry, elections and daily turns.

The development scenarios in `scripts/` that run on Anvil deploy these contracts to a local chain
and drive them with the trading server.

## License

Licensed under the Apache License, Version 2.0, whose text is in [LICENSE](../LICENSE).
