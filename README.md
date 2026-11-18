# monkeytrading

A trading engine for contracts for difference (CFDs) that runs inside a confidential-computing
enclave and publishes every command it processes.

You trade crypto-assets, currencies, stock indices, commodities and US shares against the house, and
the house's rules are the code in this repository. The engine is a deterministic state machine, the
node publishes each command it sequences with the events it produced, and the enclave signs a Merkle
root over the whole state at each checkpoint, so anyone can replay the log and check the state they
are shown. Collateral is USDC in a vault contract on Arbitrum One, which pays withdrawals against
the enclave's signed approvals and lets every account exit if the engine stops.

## Status

monkeytrading has not launched and holds no real funds.

| Part | State |
| --- | --- |
| Engine and node | Run end to end on one machine |
| Crypto prices | Live from Coinbase and Kraken |
| FX, index, commodity and share prices | Run against local stand-ins for Massive, TraderMade, CME Group and Alpaca; the vendor accounts are not in place |
| Custody vault, MONKEY, staking and operator rotation | Run on local Anvil chains; no contract is deployed |
| Account screening | Runs against a synthetic source; the production source is not chosen |
| Enclave | The SEV-SNP image builds reproducibly and the verifier passes test-signed quotes; the image has not yet run on SEV-SNP hardware |

## How it works

The values below are the [launch configuration](config/production-genesis.toml).
[`crates/core`](crates/core/README.md) states every rule in full, and [SECURITY.md](SECURITY.md)
what each part of the system is trusted for.

### Prices and orders

Every order trades against the house at the market's index: the median of the mids its venues quoted
in the last second. A market has no price, and nothing in it fills, while fewer than two venues have
quoted in that second, while their quotes spread wider than its divergence bound, in basis points
(bps, hundredths of a percent) of the index, or while its exchange is closed.

| Markets | Venues | Hours | Divergence bound |
| --- | --- | --- | --- |
| BTC-USD, ETH-USD, SOL-USD | Coinbase, Kraken | Always open | 25 bps |
| EUR-USD, GBP-USD, USD-JPY, AUD-USD | Massive, TraderMade | Sunday 17:00 to Friday 17:00, New York | 5 bps |
| US500, US100, US30, JP225, XAU-USD, XAG-USD, WTI | Massive, CME Group | CME Globex; JP225 Osaka Exchange | 10 bps |
| 20 US shares, from AAPL to ORCL | Massive, Alpaca | 09:30 to 16:00 New York, NYSE holidays | 10 bps |

Indices and commodities price from their front futures contract, adding each roll's gap so a roll
does not move the price. Accounts are kept in US dollars; JP225 converts its yen at the engine's
USD-JPY index, so it has no price while USD-JPY has none.

A market order is refused if it would fill worse than the price you were quoted by more than your
slippage bound: at most 25 bps on crypto, 5 on FX, 10 on indices and commodities, and 15 on shares.
A limit order fills once its price, spread included, is at or better than its limit, and a stop
order fills at the live price once the index reaches its stop. An order resting while its market is
closed fills at the first price after the open, gap included.

A position can carry a take profit and a stop loss, set on the order that opens or adds to it or
later. Each closes the whole position once the price you would close it at, spread included, reaches
it: a stop loss at that price, gap included, and a take profit at its level or better. A level must
sit at least the market's minimum distance from that price, proposed as its close ceiling below, and
a market's levels take one of your resting-order slots.

### Spreads

A buy fills at the index plus a spread, and a sell at the index less it. The size spread is charged
by band of position notional: each band's rate applies to the part of your position inside it, on
the part a fill opens and on the part it closes, so a round trip pays it twice. In basis points:

| Markets | First band | Second band | Above |
| --- | --- | --- | --- |
| BTC-USD | 2.0 to $2M | 4.0 to $10M | 8.0 |
| ETH-USD | 3.0 to $1M | 6.0 to $5M | 12.0 |
| SOL-USD | 2.0 to $1M | 4.0 to $5M | 8.0 |
| EUR-USD, GBP-USD (proposed) | 0.4 to $10M | 0.8 to $50M | 1.6 |
| USD-JPY, AUD-USD (proposed) | 0.5 to $10M | 1.0 to $50M | 2.0 |
| US500, US30 (proposed) | 0.7 to $5M | 1.4 to $25M | 2.8 |
| US100 (proposed) | 0.9 to $5M | 1.8 to $25M | 3.6 |
| JP225 (proposed) | 1.0 to $5M | 2.0 to $25M | 4.0 |
| XAU-USD, XAG-USD, WTI | 2.0 to $2M | 4.0 to $10M | 8.0 |
| AMD, AVGO, LLY, NVDA, ORCL, TSLA | 3.0 to $500k | 6.0 to $2M | 12.0 |
| The other 14 shares | 2.0 to $500k | 4.0 to $2M | 8.0 |

The first band's bound shown is the one at volume tier 2; your tier moves it (see [Volume
tiers](#volume-tiers)). Stress and the toxicity add-on widen the spread, but the part of a fill that
closes a position never pays more than its market's close ceiling, from 17 bps on FX to 35 bps on
ETH-USD and SOL-USD.

### Profit fee

A losing close pays nothing. A winning close pays a share of the gain it realises, after both fills'
spreads, that falls with how long the position was held:

| Hold | ≤ 10 s | 16 s | 25 s | 39 s | 1 min | 1.6 min | 2.5 min | 3.9 min | 6.1 min | 9.6 min | ≥ 15 min |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Fee | 100% | 91% | 82% | 73% | 64% | 55% | 46% | 37% | 28% | 19% | 10% |

Between points the fee falls linearly, rounded up to the basis point; from 15 minutes your tier's
base applies. An add moves the open time to the size-weighted average of when the units were added,
counting units already held as no more than 15 minutes old, so an add neither restarts the clock nor
borrows a small old position's age. A reversal closes the whole position at its hold.

### Financing

Each day at 22:00 London time, weekends included, every open position pays or receives a day's
financing on its notional: a long pays the benchmark rate plus the markup, and a short receives the
benchmark less the markup, paying when the markup is the larger. A market whose benchmark has no
published rate for the day accrues nothing.

| Markets | Benchmark | Markup | Day count |
| --- | --- | --- | --- |
| Crypto | SOFR | 5% | 365 |
| EUR-USD, GBP-USD, USD-JPY, AUD-USD | €STR, SONIA, SOFR, AONIA | 1% | 360, 365, 360, 365 |
| US500, US100, US30, commodities, shares | SOFR | 2.5% | 360 |
| JP225 | TONA | 2.5% | 365 |

### Volume tiers

Your tier comes from the volume you traded over the last 30 UTC days, no day counting for more than
the threshold divided by the days the tier needs. Opening notional counts once held for 60 seconds,
and a close counts 5 seconds after it fills unless you reopen at the same index within those 5
seconds, so churn and round trips count nothing. Every account starts at tier 0.

| Tier | 30-day volume | Days needed | First spread band ends at | Leverage | Resting orders | Requests a second | Profit fee base | Markup off | MONKEY weight |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | None | | 25% of its bound | At most 20x | 25 | 2 | 10% | | 1x |
| 1 | $1M | 2 | 50% | At most 50x | 50 | 5 | 10% | | 1.1x |
| 2 | $25M | 5 | 100% | The market's own | 100 | 10 | 9% | 10% | 1.25x |
| 3 | $250M | 10 | 250%, at most the second bound | The market's own | 250 | 20 | 8% | 20% | 1.5x |

The leverage caps raise each margin band of the initial requirement to at least 5% and 2%. A falling
tier cancels and closes nothing; it only refuses new orders past its limits and margin.

### Margin and close-outs

Margin is shared across your positions and charged by band of each position's notional:

| Markets | Margin |
| --- | --- |
| BTC-USD, ETH-USD | 1% to $2M, 2% to $10M, 5% above |
| SOL-USD | 10% to $1M, 20% above |
| FX | 0.5% to $5M, 1% to $25M, 3% above |
| Indices | 0.5% to $5M, 1% to $25M, 5% above |
| Commodities | 1% to $2M, 5% above |
| Shares | 10% to $500k, 20% to $2M, 50% above |

A fill that adds risk, and a withdrawal, must leave your equity at or above the initial requirement:
these bands, raised to your tier's floor. A fill must also keep the house's net exposure within its
cap, at least $3.16M to $31.6M per market and $61M in all. When equity falls below half the
requirement at the market's own bands, the engine closes your largest losing position that can
trade, then the next, until you are back above it. A position in a closed or unpriced market waits
until it prices. A loss beyond your balance falls on the house, never on you.

Over any closure of at least 40 hours, WTI's bands charge at least 7.5%, JP225's and XAG-USD's 1.5%,
and US100's 1%. From two hours before such a closure until the market reopens, new risk in any
market and withdrawals must meet this weekend margin, and when the closure starts, an account below
it is closed out in that market at the last price.

### Toxicity and stress

The engine marks each fill by how far the index moved its way over the next 5 seconds, in units of
the market's typical 5-second move over the last hour. Your toxicity score is how many standard
errors your recent edge sits above zero, each fill weighted by size (at most four times your
average) and the weights halving every 7 days. Once the score rests on 20 fills, a score of 4 adds 2
bps to every fill and a score of 8 adds 5 bps. Round trips at an unchanged price and close-outs
count for nothing.

Each market also has a stress level, which rises as soon as one of its triggers fires and steps down
5 minutes after the last one clears. The close ceiling holds at every level.

| Level | Size spread | Added | Fires when |
| --- | --- | --- | --- |
| 0 | 1x | None | |
| 1 | 2x | 2 bps | The venues diverge by more than 60% of the market's bound, the index moves 0.75% within a minute, or the house's net exposure in the market reaches 80% of its cap |
| 2 | 4x | 10 bps | The venues diverge by 95% of the bound, the index moves 2% within a minute, the house's exposure reaches 95% of its cap, or, in every market at once, the house has lost money to its clients over the last 24 hours |

### Deposits, withdrawals and MONKEY

A deposit of 10 to 1,000,000,000 USDC becomes trading collateral once a majority of the node's RPC
providers see it confirmed on Arbitrum, and withdrawable once its block is final on Ethereum. Before
an account's first deposit credits or its first order, it signs an eligibility declaration and the
node screens it and the client's address. A withdrawal of at least 10 USDC joins a public queue,
first in first out, paid in part when the vault is short; once a checkpoint the vault accepted holds
the approval, the enclave signs it, and anyone can submit it to the vault within 24 hours.

If checkpoints stop, the vault refuses deposits after a day, and after 14 days anyone can start its
exit against the last posted root: for 30 days each account registers what that root proves it is
owed, and the vault pays the claims pro rata. `engine exit-proof` builds the proofs from the log.

MONKEY is a token the vault mints. Every 10 minutes a budget of it, 100,000 at launch and halving
every 182.5 days, is shared among the accounts that realised losses in that period, in proportion to
those losses. Staked MONKEY receives, at each daily close, the house's net asset value above a
$30.5M floor, as far as house cash covers it, in USDC; unstaking takes 28 days.

### Who can change the rules

The operator lists markets, signs benchmark rates, schedules dividends and splits, and can stop new
risk in a market, but nothing stops you reducing or closing a position in an open, priced market.
Each of these is a signed command in the public log. The profit fee, spreads, margins, markups,
stress settings, tier table and weekend margins change only after a seven-day public notice. The fee
stays between its launch value and 1.5 times it, spreads between their launch rates and twice them,
margins at or above launch, and tier benefits at or below it. Once rotation is on, the operator is
drawn each day from a set MONKEY stakers elect, takes a fifth of each distribution, and changes
those values only by a vote of more than two thirds of the set's stake.

A log records the protocol version each rule set took effect at, so an old log replays to the roots
it published; the [version list](crates/core/src/protocol/versions.rs) says what each one changed.

## Getting it

You need Rust 1.96 or later on Linux or macOS. The local-chain scenario also needs Foundry 1.8.3
(`anvil`, `forge`, `cast`), `curl`, `jq` and Python 3; the browser verifier needs rustup with Rust
1.99.1 and its `wasm32-unknown-unknown` target, wasm-bindgen-cli 0.2.129, and Node 18 or later.

```shell
git clone https://github.com/monkey-trading/engine.git
cd engine
cargo build -p engine-node
```

This builds the `engine` binary into `target/debug/engine`. There are no releases yet.

## What you get

| Part | What it does |
| --- | --- |
| `engine run` | Runs a node: sequences commands, writes and serves the log, signs checkpoints, and serves the API. `engine run --config config/dev.toml` |
| `engine replay` | Replays a published log and checks every checkpoint's root and signature. `engine replay --log data/dev/log` |
| `engine verify-attestation` | Checks an enclave's attestation against a release manifest. `engine verify-attestation --attestation attestation.json --release release.toml --processor Genoa` |
| `engine verify-response`, `engine verify-frames` | Check a signed API response, or a signed stream connection, against the attested key |
| `engine exit-proof` | Builds the proofs an account exits the vault with, from a published log |
| `engine admin`, `engine ct-watch` | The operator's tools: sign admin commands and deliver secrets into the enclave, and watch Certificate Transparency logs for a certificate for the API's domain whose key no attestation names |
| [`contracts`](contracts/README.md) | The vault, the MONKEY token, staking and the operator registry, as a Foundry project |
| [`crates/verify-wasm`](crates/verify-wasm/README.md) | The browser verifier, an npm package |
| [`image`](image/README.md) | The reproducible SEV-SNP enclave image |

## Running it

### A local node with simulated prices

To run the engine on your machine with nothing outside it but a time source:

```shell
cargo run -p engine-node -- run --config config/dev.toml
```

This command:

- serves the API over TLS at `https://127.0.0.1:8080` with a self-signed certificate, and Prometheus
  metrics at `http://127.0.0.1:9464/metrics`;
- mocks the enclave, custody and feeds: three simulated venues quote eight markets (BTC-USD,
  ETH-USD, EUR-USD, US500, DE40, XAU-USD, WTI and AAPL), each on its exchange's hours;
- writes the log to `data/dev/log` and the account history index to `data/dev/history`;
- sequences nothing until two of three public Roughtime servers agree on the time over UDP, and
  stops when 60 seconds pass without such a sample.

It has worked when the node logs `first trusted time sample: sequencing started` and, in another
terminal, this prints each market, whether it has a price, and its last price (`mark`); AAPL is
`false` outside NYSE hours:

```shell
curl -sk https://127.0.0.1:8080/v1/markets | jq -r '.[] | [.symbol, .pricing.priced, .mark] | @tsv'
```

This node credits no deposit and fills no order, since both need a signed eligibility declaration
screened by a source `config/dev.toml` does not name. ctrl-c stops it. A log is tied to the
`[genesis]` it started with: after editing that section, remove `data/dev`.

### With live crypto prices

To price BTC-USD and ETH-USD from Coinbase and Kraken:

```shell
mkdir -p data
sed '/^\[feeds\]/,/^\[/ s/^provider = "mock"/provider = "venues"/' config/dev.toml >data/venues.toml
cargo run -p engine-node -- run --config data/venues.toml
```

The node reads both venues' public WebSocket feeds over TLS, with no key. It has worked when the
`curl` above prints `true` for BTC-USD and ETH-USD and `false` for the six other markets.

### Against a local chain with the vault

To deposit USDC into the vault, have it credited, and trade with signed orders:

```shell
(cd contracts && forge soldeer install)
./scripts/dev-access-enforcement.sh
```

After installing the contracts' pinned dependencies, the script starts Anvil on port 8545 with a
mock USDC and the vault, three RPC proxies on 18545 to 18547, a synthetic screening source on 18846,
and a node on [`config/anvil.toml`](config/anvil.toml) with its API on 8080 and metrics on 9464, in
a temporary directory it removes. It deposits, declares and trades with signed requests, checks the
refusals screening makes, restarts the node, reorganises a deposit away and replays the log.
`ANVIL_PORT`, `API_PORT`, `METRICS_PORT` and `ACCESS_SOURCE_PORT` move it off ports you use. It has
worked when it ends with `PASS: every deposit credited or held once after finality; public log
replay verifies`.

### The verifier in a browser

To build the WebAssembly package a web page uses to check the node's attestation and signed answers
with the same Rust code as the `engine` commands:

```shell
scripts/build-wasm-verifier.sh out/verifier
./scripts/dev-wasm-verifier.sh out/verifier
```

The first command writes the `.wasm` module, its JavaScript glue and TypeScript types,
`package.json` and `SHA256SUMS` to `out/verifier`, failing past 1.5 MB gzipped; on x86_64 Linux it
builds the same bytes from any checkout. The second checks the package under Node against the
`engine` CLI on every attestation fixture and signed-answer vector, and has worked when it prints
`PASS: wasm and native verdicts match on <n> fixtures`. The package's
[README](crates/verify-wasm/README.md#usage) shows a page calling it.

## Talking to it

The node serves a REST API under `/v1`, and at `/v1/stream` a WebSocket stream of the log from any
`?from=<seq>` onwards, over TLS on `node.listen` within the limits `[api]` sets.
[`crates/node/README.md`](crates/node/README.md#usage) lists every endpoint and its body.

- **Reads:** `/v1/markets`, `/v1/accounts/{address}`, `/v1/house`, `/v1/status`, and an account's
  proof against the last checkpoint at `/v1/accounts/{address}/proof`.
- **Writes:** `POST /v1/orders`, `/v1/orders/cancel`, `/v1/withdrawals` and the rest carry the
  account's EIP-712 signature, its next nonce and an expiry at most 5 minutes ahead. An account can
  register a session key at `/v1/session-keys` to place and cancel orders for up to 24 hours.
- **Signed answers:** send `X-Engine-Challenge` with 32 random bytes in hex, and the answer carries
  the enclave key's signature over your request, the challenge, the node's time and log head, and
  the body. `/v1/stream?signed=1&challenge=<hex>` signs every frame, with a heartbeat every 5 s.

## Verifying it

**The log.** Replay re-applies every command with this repository's code and checks each
checkpoint's state root and signature, so a node cannot report a state its log does not produce:

```shell
cargo run -p engine-node -- replay --log data/dev/log
```

It prints `verified: <n> commands, <n> checkpoints, final root 0x…` and the checkpoint signers.
`--log` also takes a copy of a node's `/v1/archive/`, and `--attestation` requires every signer to
be attested.

**The enclave.** An attestation proves which code holds the signing key:

```shell
curl -sk https://127.0.0.1:8080/v1/attestation > attestation.json
cargo run -p engine-node -- verify-attestation --attestation attestation.json \
  --release release.toml --processor Genoa
```

`release.toml` is the manifest each release will publish, and `--processor` the host's AMD CPU
generation. The check passes only if AMD's chain, to a root pinned in the binary, signed the quote;
the launch measurement is the release's image, which [anyone can rebuild](image/README.md);
debugging is off; the firmware meets the release's minimum; and the report binds the signing
address, a config file the release lists, and the API's TLS key. Against the local node it prints
`the quote is not an SEV-SNP attestation report; a mock quote never verifies`.

**A signed answer.** The local node's deployment is chain 31337 and the zero vault address:

```shell
API=https://127.0.0.1:8080
CHALLENGE=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
curl -sk -D headers -o body -H "X-Engine-Challenge: $CHALLENGE" $API/v1/markets
cargo run -p engine-node -- verify-response \
  --signer "$(curl -sk $API/v1/attestation | jq -r .enclave_address)" \
  --chain-id 31337 --vault 0x0000000000000000000000000000000000000000 \
  --challenge $CHALLENGE --path /v1/markets --headers headers --body body
```

It prints `verified: status 200, signed by 0x…`, and refuses a changed byte with `the signature is
not the attested key's over these bytes`. Against a real node, take `--signer` from the attestation
you verified. **A signature cannot stop someone in between from delaying an answer or sending your
request first, so confirm a refused order or withdrawal with a fresh signed read of the account.**

## Contributing

Pull requests against `main` are welcome; for a large change, open an issue first.

- Run `cargo fmt`, keep `cargo clippy --workspace --all-targets -- -D warnings` clean, test the
  crates you touch, and keep Rust files within 500 lines (`./scripts/check-file-loc.sh`). For the
  contracts, run `forge fmt --check`, `forge build` and `forge test` from `contracts/`.
- `engine-core` stays deterministic: no clocks, randomness, floating point, `HashMap` iteration or
  IO, and money is `Fixed`. A change to its rules, state or events ships as a new protocol version
  ([Adding a protocol version](crates/core/README.md#adding-a-protocol-version)).
- Title commits and pull requests `[<scopes>] Subject`, naming the areas touched, for example
  `[core,node] Cap the spread a close can pay`.

## Security

Report a vulnerability privately, through GitHub's private vulnerability reporting or to
william.jevons@proton.me, never in a public issue. [SECURITY.md](SECURITY.md) gives the scope, what
to include and the response times.

## License

All of monkeytrading, the contracts included, is licensed under the Apache License 2.0; see
[`LICENSE`](LICENSE).
