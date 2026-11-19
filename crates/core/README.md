# engine-core

The deterministic state machine that holds every account and market and enforces the trading rules.

`engine-core` is the engine itself. It takes the commands that the trading server has put in
order, such as orders, venue prices, deposits, withdrawals, operator instructions and clock ticks.
It applies each command to its state and returns the events that the command produced. After any
command it can compute the state root, a keccak-256 Merkle root over the entire state.

The crate performs no input or output. It reads no clock, uses no random numbers and does no
floating-point arithmetic. Time enters only as the timestamp that the server stamps on each
command, and every amount of money is a fixed-point integer (`Fixed`). Anyone can therefore replay
a published log against the crate and reproduce every state root that the enclave signed.

`engine-node` runs the engine inside the enclave and replays published logs with it.
[The trading rules](../../docs/trading-rules.md) set out the rules that the engine enforces.

## Usage

A caller creates an engine, applies each sequenced command in order, and reads the state root:

```rust
use engine_core::Engine;
use engine_core::command::{Command, Sequenced};

let mut engine = Engine::new();
let events = engine.apply(&Sequenced { seq: 0, ts: 0, cmd: Command::Tick })?;
let root = engine.state_root();
```

The other entry points are on `Engine` too:

- `Engine::account_proof` proves an account's balance and positions, and the markets it holds
  positions in, against the last state root. `AccountProof::verify` checks such a proof against a
  root taken from a signed checkpoint. The layout suits a Solidity verifier, and
  `src/merkle/mod.rs` documents it.
- `Engine::quote` prices an order with the same functions that execute it, without changing state.
- `Engine::exit_proof` builds the proof that an account presents to the vault's exit.

The rules are versioned. A log activates each protocol version at a recorded sequence number, and
each command applies under the version in force at that point. An old log therefore replays under
the rules that applied when it was written. [`src/protocol/versions.rs`](src/protocol/versions.rs)
lists every version with a summary of its changes.

Clients sign each request under an EIP-712 domain bound to the chain and the vault, with the
account's next nonce and an expiry no more than five minutes ahead. The `sign_request` example signs
a request as a wallet would. Run on the first request in the signing fixture, it prints the request
body with its `auth` object added:

```shell
SIGNER_KEY=$(jq -r .private_key crates/core/tests/fixtures/eip712.json) \
  cargo run -p engine-core --example sign_request -- \
  31337 0x5FbDB2315678afecb367f032d93F642f64180aa3 0 1792540800000 order \
  "$(jq -c '.requests[0].body' crates/core/tests/fixtures/eip712.json)"
```

The signature in the output equals the fixture's `signature` for that request. The
`verify_proof` example, documented in `examples/verify_proof.rs`, checks an account proof from a
server against a root without trusting the server.

## Layout

| Area | Modules | Responsibility |
| --- | --- | --- |
| State and dispatch | `engine/`, `command/`, `event/`, `account`, `market/`, `params/`, `types` | The engine's state, the commands it accepts, the events and refusals it returns, accounts, market listings and protocol parameters |
| Prices and calendars | `price`, `marks`, `session/`, `futures`, `benchmark`, `corporate/` | The multi-venue reference price, marks, trading hours, futures rolls, dated benchmark rates, and dividends and splits |
| Orders and charges | `execution/`, `spread_tier/`, `close_spread`, `stress`, `holding_fee/`, `volume_tier/`, `levels/` | Order placement and execution, spread bands, the closing spread cap, stress levels, the profit fee, volume tiers, and take profit and stop loss |
| Risk | `closeout`, `closeout_index/`, `weekend/`, `portfolio/`, `house_loss/`, `hot_indexes/` | Margin and close-out, weekend margin, portfolio margin, which close-outs read from `V81` and new risk and withdrawals from `V84`, the house loss trigger, and the indexes that find the accounts and orders a price affects |
| Toxicity | `markouts`, `toxicity/` | Markouts after each trade and the toxicity score built from them |
| Funds and access | `funds`, `financing/`, `deposits/`, `withdrawal`, `access/`, `auth`, `eip712`, `session_keys` | Deposits, overnight financing and the root over each account's charges, withdrawals and their queue, eligibility and screening, signed requests and session keys |
| Operator and governance | `admin/`, `operator_bounds/`, `listing_bounds/`, `notice/`, `governance/`, `election`, `turns/`, `operator_faults`, `protocol/` | Operator instructions and the bounds on them, notices, votes, the elected operator, daily turns, fault penalties and protocol versions |
| MONKEY | `emissions`, `staking/` | MONKEY emissions and the USDC distributions to stakers |
| State tree | `merkle/`, `tracked`, `fixed/` | The state tree in versions that a node can feed in parts, its proofs, change tracking for the root, and fixed-point arithmetic |

Each module's own documentation describes how it works. Protocol tests are in `tests/`, grouped
by area, with the shared harness in `tests/common/`. `tests/reference/` holds one reference log per
protocol version.

## Configuration

The crate reads no configuration, and the `sign_request` example reads the account's private key
from `SIGNER_KEY`. The crate has two features:

- `short-turns` makes an operator's turn five minutes long instead of a day, for development
  stacks. A log written under this feature replays only under it, and a production build of the
  trading server refuses it.
- `closeout-oracle` adds test hooks to the close-out index. The crate's own tests turn it on.

## Testing

```shell
cargo test -p engine-core
```

This command runs the unit, protocol and property tests. CI also runs two of them on x86_64 and
aarch64 Linux, because they guard replay:

- `cargo test -p engine-core --test reference_logs` replays every log in `tests/reference/` and
  fails at the first sequence number whose state root differs from the committed one.
  `scripts/check-reference-logs.sh` fails a branch that changes, renames or deletes one of them.
- `cargo test -p engine-core --test invariants` runs 1,024 generated sessions from a fixed seed and
  checks the accounting invariants after every command.

The latency benchmarks are ignored by default and need a quiet machine. `hot_path_benchmark`,
`launch_book_benchmark`, `mass_events_benchmark` and `portfolio_margin_benchmark` in `tests/` each
give their command and their budgets in their documentation. For example, the following command
times the engine's hot path:

```shell
cargo test -p engine-core --test hot_path_benchmark --locked -- --ignored --nocapture --test-threads=1
```

## Adding a protocol version

A change to the engine's state, to how the state is serialised, or to what a command does changes
the state roots that existing logs replay to. Such a change ships as a new protocol version, and the
reference log of every earlier version must still replay unchanged. A change to the events that a
command emits is also a protocol change, even when the state roots stay the same.
`engine-node`'s reference logs carry each command's events, and `engine replay` refuses a log whose
events differ from those the engine emits.

To add a version:

1. Add the version in code, following the steps in the documentation of
   [`src/protocol.rs`](src/protocol.rs).
2. Write the version's reference session as `examples/reference_log/v<N>.rs`. Declare it in
   `examples/reference_log/main.rs`, and call its `session` function at the end of
   `examples/reference_log/session.rs`, so that the session activates the new version and exercises
   its rules.
3. Write the version's reference log, where `<N>` is the new version's number. The generator
   refuses to overwrite an existing file.

   ```shell
   cargo run -p engine-core --example reference_log -- crates/core/tests/reference/v<N>.jsonl
   ```

4. Write `engine-node`'s reference log from it, which adds the events each command emits:

   ```shell
   cargo run -p engine-node --example reference_log -- \
     crates/core/tests/reference/v<N>.jsonl crates/node/tests/reference/v<N>.jsonl
   ```

5. Commit both files. The replay tests and CI find them with no other change.

Every log starts under `V2`, so `v1.jsonl` and `v2.jsonl` both replay under it.

## License

Licensed under the Apache License, Version 2.0, whose text is in [LICENSE](../../LICENSE).
