# monkeytrading

An engine for trading contracts for difference (CFDs), built to run inside a confidential-computing
enclave and to publish every instruction it processes.

A contract for difference lets you take a long or short position on a market's price without owning
the asset. On monkeytrading you trade crypto-assets, currencies, stock indices, commodities and US
shares against the house, which takes the other side of every trade. What sets monkeytrading apart
from a conventional broker is that the house's rules are the code in this repository.

The engine runs inside an enclave, a virtual machine whose memory the host machine can neither read
nor alter. The enclave's processor signs an attestation, a report that identifies the exact code the
enclave is running. The trading server publishes every instruction it processes, together with its
effects. At regular intervals, called checkpoints, the enclave signs a fingerprint of the entire
state, known as a Merkle root. Anyone can replay the published record and confirm that the state
they are shown is the one the rules produce.

Collateral is held as USDC in a vault contract on Arbitrum One. The vault pays the withdrawals that
the enclave approves. If the engine stops, the vault lets every account recover its funds.

## Status

monkeytrading has not launched and holds no real funds.

| Part | State |
| --- | --- |
| Engine and trading server | Runs end to end on a single machine |
| Crypto prices | Live from Coinbase and Kraken |
| FX, index, commodity and share prices | Tested against local stand-ins for Massive, TraderMade, CME Group and Alpaca; the vendor accounts are not yet in place |
| Custody vault, MONKEY, staking and operator rotation | Tested on local Anvil chains; no contract has been deployed |
| Account screening | Tested against a synthetic source; the production source has not been chosen |
| Enclave | The SEV-SNP image builds reproducibly, and its verifier accepts test-signed reports; the image has not yet run on SEV-SNP hardware |

## How it works

This section summarises the rules a trader meets, and [the trading rules](docs/trading-rules.md) set
out each of them in full, with the values the engine launches with. [SECURITY.md](SECURITY.md)
states what each part of the system is trusted for.

### Prices and orders

Each market has a reference price, which the engine calculates from the quotes of independent
venues. The reference price is the median of the mid prices that the market's venues have quoted
within the last second. A market can be traded only while at least two of its venues have quoted
within that second and their quotes agree to within the market's divergence limit. When either
condition fails, or when the market's exchange is closed, the market has no price and no order in it
can be executed. The divergence limit is measured in basis points, abbreviated to bps. One basis
point is one hundredth of one percent.

| Markets | Venues | Trading hours | Divergence limit |
| --- | --- | --- | --- |
| BTC-USD, ETH-USD, SOL-USD | Coinbase, Kraken | Always open | 25 bps |
| EUR-USD, GBP-USD, USD-JPY, AUD-USD | Massive, TraderMade | Sunday 17:00 to Friday 17:00, New York time | 5 bps |
| US500, US100, US30, JP225, XAU-USD, XAG-USD, WTI | Massive, CME Group | CME Globex sessions; JP225 follows Osaka Exchange | 10 bps |
| 20 US shares, from AAPL to ORCL | Massive, Alpaca | 09:30 to 16:00 New York time, excluding NYSE holidays | 10 bps |

Every order is executed against the house at the reference price, plus a spread for a buy or less a
spread for a sell. A market order is rejected if its execution price would be worse than the price
you were quoted by more than your maximum slippage. Limit orders, stop orders, and the take profit
and stop loss levels you can attach to a position are executed when the price reaches them. A stop
loss may be executed at a worse price than its level if the market gaps.

### Costs

Every trade pays a spread, which is charged when you open a position and again when you close it.
The spread rises in bands as a position grows. On BTC-USD, for example, an account at volume tier 2
pays 2 bps on the first $2 million of a position, 4 bps on the part up to $10 million, and 8 bps on
any part above that.

No fee is charged on a losing trade. On a winning trade, a share of the realised profit is charged
as a fee, after the spreads paid on opening and closing. The fee falls the longer the position has
been held, from 100% on a position closed within 10 seconds of opening to 10% on one held for 15
minutes or more.

A position held through the daily rollover at 22:00 London time pays or receives a day's financing.
A long position pays a benchmark interest rate plus a markup, and a short position receives the
benchmark rate less the markup.

Your volume tier depends on the trading volume you have built up over the last 30 days. A higher
tier extends the band in which you pay the lowest spread and raises your maximum leverage. Tiers 2
and 3 also reduce the profit fee and the financing markup.

Two charges can widen the spread. Accounts whose trades are consistently followed by price moves in
their favour pay a toxicity add-on of 2 or 5 bps. Each market also has a stress level, which widens
its spreads during turbulent conditions. Neither charge can raise the spread on closing a position
above the market's closing spread cap, which ranges from 17 bps on FX to 35 bps on ETH-USD and
SOL-USD.

### Margin and close-out

Margin is shared across all the positions in your account. Each market sets its margin as a
percentage of position value, rising in bands as the position grows. On BTC-USD and ETH-USD, the
margin on the first $2 million is 1%, which allows leverage of up to 100 times from volume tier 2.

The close-out level is half of the margin requirement. If your equity falls below the close-out
level, the engine closes your largest losing position that can be traded, and then the next, until
your equity recovers. Any loss beyond your balance is borne by the house, not by you.

### Funds and MONKEY

Deposits and withdrawals are made in USDC through the vault. Before an account's first deposit is
credited or its first order is accepted, the account must sign an eligibility declaration.
Withdrawals join a public first-in, first-out queue. The vault pays each withdrawal once the enclave
has signed an authorisation for it. If the enclave stops posting checkpoints for 14 days, anyone can
start the vault's exit. The exit pays each account what the last checkpoint shows it is owed.

MONKEY is a token minted by the vault. Every ten minutes, a fixed amount of MONKEY is distributed
among the traders who realised a loss in that period, in proportion to the size of their losses.
Staked MONKEY earns a daily share, paid in USDC, of the house's net asset value above a reserve of
$30.5 million.

### Changes to the rules

The operator, who runs the trading server, lists markets, publishes benchmark interest rates and can
suspend new positions. No operator action can prevent you from reducing or closing a position in a
market that is open and has a price. Fees, spreads, margins and the other trading settings can
change only after seven days' public notice. Each change to the rules takes effect as a new protocol
version, so that an old log still replays under the rules that applied when it was written. The
[version list](crates/core/src/protocol/versions.rs) describes each version.

## Getting it

You need Rust 1.96 or later, on Linux or macOS. The local-chain example also needs Foundry 1.8.3,
`curl`, `jq` and Python 3. The browser verifier needs rustup with Rust 1.99.1 and its
`wasm32-unknown-unknown` target, wasm-bindgen-cli 0.2.129, and Node 18 or later.

```shell
git clone https://github.com/monkey-trading/engine.git
cd engine
cargo build -p engine-node
```

These commands build the `engine` binary into `target/debug/engine`. No release has been published
yet.

## What you get

| Part | Purpose |
| --- | --- |
| `engine run` | Runs the trading server, which processes instructions, writes and publishes the log, signs checkpoints and serves the API. Example: `engine run --config config/dev.toml` |
| `engine replay` | Replays a published log and checks the state fingerprint and signature at every checkpoint. Example: `engine replay --log data/dev/log` |
| `engine verify-attestation` | Checks an enclave's attestation against a release manifest. Example: `engine verify-attestation --attestation attestation.json --release release.toml --processor Genoa` |
| `engine verify-response`, `engine verify-frames` | Check a signed API response, or a signed stream connection, against the attested key |
| `engine exit-proof` | Builds, from a published log, the proofs an account needs to recover its funds through the vault's exit |
| `engine admin`, `engine ct-watch` | The operator's tools, which sign administrative instructions and watch Certificate Transparency logs for a certificate that no attestation names |
| [`contracts`](contracts/README.md) | The vault, the MONKEY token, staking and the operator registry, as a Foundry project |
| [`crates/core`](crates/core/README.md) | The engine itself, a deterministic state machine that enforces the trading rules |
| [`crates/verify-wasm`](crates/verify-wasm/README.md) | The browser verifier, published as an npm package |
| [`image`](image/README.md) | The reproducible SEV-SNP enclave image |

## Running it

### A local server with simulated prices

To try the engine on your own machine, with simulated prices and no external service other than a
time source, run:

```shell
cargo run -p engine-node -- run --config config/dev.toml
```

This command:

- serves the API over TLS at `https://127.0.0.1:8080`, with a self-signed certificate, and
  Prometheus metrics at `http://127.0.0.1:9464/metrics`;
- simulates the enclave, custody and the price feeds, with three simulated venues quoting eight
  markets (BTC-USD, ETH-USD, EUR-USD, US500, DE40, XAU-USD, WTI and AAPL);
- writes the log to `data/dev/log` and the account history to `data/dev/history`;
- processes nothing until two of three public Roughtime servers agree on the time.

The server is running once its log shows `first trusted time sample: sequencing started`. In another
terminal, the following command lists each market, whether it currently has a price, and its last
reference price, which the API calls `mark`:

```shell
curl -sk https://127.0.0.1:8080/v1/markets | jq -r '.[] | [.symbol, .pricing.priced, .mark] | @tsv'
```

AAPL shows `false` outside New York Stock Exchange hours. This server does not accept deposits or
orders, because both require a screening source that `config/dev.toml` does not configure. Press
Ctrl-C to stop the server. The log is bound to the `[genesis]` section it was started with. If you
edit that section, remove `data/dev` before you start the server again.

### With live crypto prices

To price BTC-USD and ETH-USD from Coinbase and Kraken, create a copy of the configuration that uses
live venues, and start the server with it:

```shell
mkdir -p data
sed '/^\[feeds\]/,/^\[/ s/^provider = "mock"/provider = "venues"/' config/dev.toml >data/venues.toml
cargo run -p engine-node -- run --config data/venues.toml
```

The server connects to both venues' public feeds without an API key. Once the server is running, the
`curl` command above shows `true` for BTC-USD and ETH-USD and `false` for the other six markets.

### Against a local chain with the vault

To deposit USDC into the vault, have the deposit credited and trade with signed orders, install the
contracts' dependencies and run the funding and trading scenario:

```shell
(cd contracts && forge soldeer install)
./scripts/dev-access-enforcement.sh
```

The scenario starts:

- Anvil on port 8545, with a mock USDC token and the vault;
- a synthetic screening source on port 18846, and three RPC proxies on ports 18545 to 18547;
- a server using [`config/anvil.toml`](config/anvil.toml), with its API on port 8080.

The scenario then deposits funds, places signed orders, restarts the server and replays the log. All
of its data is kept in a temporary directory. To use other ports, set `ANVIL_PORT`, `API_PORT`,
`METRICS_PORT` and `ACCESS_SOURCE_PORT`. The scenario has succeeded when its last line reads
`PASS: every deposit credited or held once after finality; public log replay verifies`.

### The verifier in a browser

A web page can check the server's attestation and signed answers itself, using a WebAssembly build
of the same Rust code that the `engine` commands run. To build the package and compare it with the
command-line tools, run:

```shell
scripts/build-wasm-verifier.sh out/verifier
./scripts/dev-wasm-verifier.sh out/verifier
```

The first command writes the package to `out/verifier`. The second runs the package under Node
against every test fixture, and compares its verdicts with those of the `engine` command-line tools.
The check has passed when the second command prints
`PASS: wasm and native verdicts match on <n> fixtures`. The package's
[README](crates/verify-wasm/README.md#usage) shows a web page calling it.

## Talking to it

The server provides a REST API under `/v1` and a WebSocket stream at `/v1/stream`, both over TLS on
the address that `node.listen` sets. Requests that act for an account, such as orders and
withdrawals, must carry the account's EIP-712 signature, its next nonce, and an expiry no more than
five minutes ahead. When a request carries an `X-Engine-Challenge` header of 32 random bytes in
hexadecimal, the enclave signs the answer together with the request and the challenge.
The [API reference](docs/api.md) lists every endpoint and request body.

## Verifying it

**The log.** Replay applies every instruction in a log with this repository's code, and checks the
state fingerprint and signature at each checkpoint. If a server reports a state that its own log
does not produce, replay exposes the difference.

```shell
cargo run -p engine-node -- replay --log data/dev/log
```

Replay prints `verified: <n> commands, <n> checkpoints, final root 0x…`, followed by the keys that
signed the checkpoints. With `--attestation`, replay also requires every checkpoint to be signed by
a key that a verified attestation names.

**The enclave.** The attestation shows which code holds the signing key.

```shell
curl -sk https://127.0.0.1:8080/v1/attestation > attestation.json
cargo run -p engine-node -- verify-attestation --attestation attestation.json \
  --release release.toml --processor Genoa
```

`release.toml` is the manifest that each release will publish, and `--processor` names the
generation of the host's AMD processor. The check passes only if AMD's certificate chain signed the
report, the launch measurement matches the release's image, and the report binds the signing key and
a configuration that the release lists. Anyone can [rebuild the image](image/README.md) to confirm
its measurement. Against the local server, which simulates its enclave, the command prints
`the quote is not an SEV-SNP attestation report; a mock quote never verifies`.

**A signed answer.** The local server runs on chain 31337 with the zero vault address. The following
commands send a request with a fresh challenge and verify the answer:

```shell
API=https://127.0.0.1:8080
CHALLENGE=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
curl -sk -D headers -o body -H "X-Engine-Challenge: $CHALLENGE" $API/v1/markets
cargo run -p engine-node -- verify-response \
  --signer "$(curl -sk $API/v1/attestation | jq -r .enclave_address)" \
  --chain-id 31337 --vault 0x0000000000000000000000000000000000000000 \
  --challenge $CHALLENGE --path /v1/markets --headers headers --body body
```

The final command prints `verified: status 200, signed by 0x… at <time> ms, log head seq <seq>`.
The time is the server's clock when it answered, in milliseconds since the Unix epoch. The last
number is the sequence number of the latest instruction the server had recorded in its log. If a
single byte of the body has changed, the command reports
`the signature is not the attested key's over these bytes` instead.
Against a live server, take the `--signer` address from an attestation you have verified, not from
the server.

**A signature cannot prevent someone between you and the server from delaying an answer, or from
sending your request to the server first. Before you treat an order or withdrawal as refused,
confirm the refusal with a new signed read of the account.**

## Contributing

Pull requests against `main` are welcome. For a large change, please open an issue first.

- Format Rust code with `cargo fmt`, and keep
  `cargo clippy --workspace --all-targets -- -D warnings` free of warnings. Test the crates you
  change, and keep each Rust file within 500 lines.
- `engine-core` must stay deterministic. A change to its rules, state or events ships as a new
  protocol version, as [Adding a protocol version](crates/core/README.md#adding-a-protocol-version)
  describes.
- Commit and pull request titles take the form `[<scopes>] Subject`, for example
  `[core,node] Cap the spread charged on closing a position`.

## Security

Please report vulnerabilities privately, through GitHub's private vulnerability reporting or by
email to william.jevons@proton.me, and never in a public issue. [SECURITY.md](SECURITY.md) describes
the scope, what a report should include and how quickly to expect a response.

## License

monkeytrading, including its contracts, is licensed under the Apache License 2.0. The licence text
is in [`LICENSE`](LICENSE).
