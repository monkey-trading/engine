# monkeytrading

monkeytrading is an open-source engine for trading contracts for difference on crypto-assets,
currencies, stock indices, commodities, and shares. As with a conventional CFD broker, the house
takes the other side of every trade. Unlike one, its rules are public code, it runs inside a
confidential-computing enclave, and it publishes every command it processes, so anyone can replay
the record and confirm that the state it reports is the state its rules produce. Ownership of the
house, in turn, is issued to the people who trade with it.

> **Pre-launch.** The engine runs end to end on a single machine, with a mock enclave, mock
> custody, and simulated prices, or live crypto prices from three venues; vault deposits also run
> against a local chain. Signed checkpoint posting, rollback checks, and signed withdrawals with
> their FIFO queue also run against Anvil, as do MONKEY emissions and claims; MONKEY's production
> budget and start are still to be set. FX, index, commodity, and share prices from two market-data
> vendors each run against local stand-ins for those vendors; live vendor feeds wait on credentials
> and redistribution licences. MONKEY staking and hardware attestation are under construction.
> Nothing here holds real funds.

## How it works

Every order trades against the house at a reference price: the median of recent quotes from
several independent venues. A market order fills immediately at that price; a limit or stop order
rests until the price reaches it. If too few venues have quoted recently, or their quotes disagree
by more than a set margin, the market has no price and nothing fills, so there is never a stale
price to pick off. A quote shown to a client is a guide rather than a promise: a market order fills
at the live price, and is declined only if that price is worse than the quote by more than the
client's slippage bound.

Ordinary orders pay no spread. Very large positions pay a fixed number of basis points on the part
that takes them past each size band, and spreads widen at times of stress, when venues diverge,
prices move fast, or the house's exposure runs high. Accounts whose trades are consistently
followed by a move in their favour, the mark of trading on prices that are about to change, pay a
small additional spread on every trade.

Losing trades pay nothing. Winning trades pay a fixed share of the profit they realise, and
positions held over the daily rollover pay or receive financing at a benchmark interest rate plus
or minus a markup. Under V18, a separate operator worker fetches dated rates from the official
publishers. After downtime, each missed rollover uses the rate effective on its local date. The
worker setup and stale-rate policy are in [engine-node](crates/node/README.md#dated-benchmark-worker).

Accounts are kept in US dollars. A market quoted in another currency, such as Germany 40 in euros
or Japan 225 in yen, accrues profit and loss in that currency and converts it to dollars at the
engine's own price for the currency pair, the same way for margin, exposure, and financing. While
that currency pair has no price, neither does the market, and nothing trades in it.

Margin is shared across an account's positions. When equity falls below half of the margin
requirement, the largest losing position that can trade is closed first, and so on until the
account recovers; a position in a market that is closed or has no fresh price waits until that
market prices again. Losses beyond an account's balance fall on the house, never on the trader. The
check runs on each price update, whenever a price moves without one because a market opens or a
quote grows too old, after each day's financing, and after a change to a margin table or the
close-out level. A market with no fresh price allows no dealing at all, while accounts
holding it trade other markets and withdraw against its last price. Markets in real-world assets
keep their exchange's hours, holidays, and early closes, and orders resting while a market is
closed fill at the first price after it opens, gap and all; no quote from outside a market's hours
ever prices it. Indices and commodities price from their front futures contract, and when one
contract rolls to the next the price takes the gap between them, so it does not jump. The house limits its own risk by
capping its net exposure in each market, and the sum of those exposures across markets, at the
larger of a fraction of its net asset value and a fixed dollar floor, so trading can start from a
house with no capital and carries on while the house is in deficit.

Collateral is USDC, held in a vault contract on Arbitrum One. The chain safeguards funds and never
executes trades. A deposit is credited once the L2 batch holding it is posted to Ethereum
([#22](https://github.com/0xjevons/monkeytrading/issues/22)). The vault pays a withdrawal against
the enclave's signed approval ([#23](https://github.com/0xjevons/monkeytrading/issues/23)); one it
cannot cover waits in a public first-in-first-out queue
([#49](https://github.com/0xjevons/monkeytrading/issues/49)). Ownership of the house is represented
by the MONKEY token, earned on losing trades: every ten minutes a fixed budget is shared among the
accounts in proportion to their net realised losses, less any debt the house wrote off, and the
budget halves every 182.5 days. An account claims what it earned from the vault, against the
engine's latest posted checkpoint, and no entitlement mints twice
([#36](https://github.com/0xjevons/monkeytrading/issues/36)). Holders who stake it share, in USDC,
the house's net asset value above a reserve floor
([#50](https://github.com/0xjevons/monkeytrading/issues/50)).

The operator lists markets and their trading calendars, sets benchmark rates and protocol
parameters, raises stress levels, and can stop new risk in one market or in all of them at once;
neither those modes nor any spread stops a trader reducing or closing a position in an open market
with a price. Once the engine has credited any funds, each
of these is a command the operator's key signs, published in the log with its signature, and replay
checks every one.

The reasoning behind each of these choices is recorded in the decision issues
[#1](https://github.com/0xjevons/monkeytrading/issues/1) to
[#16](https://github.com/0xjevons/monkeytrading/issues/16).

## Verifying the engine

The engine is deterministic: given the same sequence of commands, it reaches the same state on any
machine, to the last unit. The node streams each command it sequences, with the events it produced,
and the enclave periodically signs a root that commits to the whole state: a Merkle tree over
accounts, markets, and the rest of the state, so each checkpoint rehashes only what changed. Any
account can fetch a proof of its balance and positions against the last checkpoint's root and check
it without the node. `engine replay` re-applies a log from the beginning and checks the root at
every checkpoint against the state the published code computes, and fails a log it cannot find or
that holds no checkpoint, has a missing or invalid signature, or names an inconsistent deployment.
Early logs that predate domain recording require explicit `--chain-id` and `--vault`; logs with
withdrawal releases require an independently trusted `--signer`. Replay reads
a node's log directory or the archive the node serves: its completed log segments, with a manifest of their seq ranges and
SHA-256 hashes. A stream subscriber can start or resume at any message offset, and gets the log from
disk and then live, each message once.

Optional `[snapshots]` configuration caches checkpoint state for restart. Recovery authenticates
each cache against independently pinned attested enclave signers and checks its recomputed root
against its logged checkpoint before replaying the suffix. Invalid caches warn and fall back to
an earlier cache or full replay. See [node recovery configuration](crates/node/README.md).

Every command carries the time the sequencer stamped it with. The enclave's host supplies its
clock, so the node checks that clock against signed time from several independent
[Roughtime](https://www.rfc-editor.org/rfc/rfc10049) servers. It stamps nothing until signed time
has arrived, and stops sequencing while the clock is further from the time the servers agree on
than a set bound, or once a minute has passed without signed time.

The enclave also attests to the key it signs with, to the key its API serves TLS with, and to the
SHA-256 of the config file it runs with. Both keys are generated inside the enclave, so client
connections end where the host cannot read them. One hash in the attestation's report data commits
to all three: the hash of a bundle the node serves beside the quote. The local node uses a mock
attestation, labelled as such. The confidential VM image for AMD SEV-SNP builds reproducibly from
[`image`](image), so anyone can rebuild a release and compare its launch measurement; booting it on
SEV-SNP hardware is being validated in [#108](https://github.com/0xjevons/monkeytrading/issues/108),
and a verifier for its quotes is being built in
[#27](https://github.com/0xjevons/monkeytrading/issues/27).

## Running it locally

You need the latest stable Rust on Linux or macOS, and `jq` and `xxd` for the API examples. Start
a local node with a mock enclave, mock custody, and simulated prices; its API is served over TLS on
`127.0.0.1:8080`, which you can change with `node.listen` in `config/dev.toml`. The certificate is
self-signed, so the examples below pass `curl -k`. Set `feeds.provider = "venues"` in a copy of the
config to price BTC-USD and ETH-USD from Coinbase, Kraken, and Bitstamp instead; the other markets
then have no price, so their orders get `no_price`. The node checks its clock against public
Roughtime servers over UDP: it starts sequencing once they answer, and stops a minute after it
loses them. A log is tied to the `[genesis]` it was started with: after editing that section, delete
`data/dev/log` before starting the node again, or it refuses to resume.

```bash
cargo test --workspace
cargo run -p engine-node -- run --config config/dev.toml
```

In another terminal, credit a test account, buy 0.25 BTC at the live price, and read the account
and its history:

```bash
API=https://localhost:8080
ACCOUNT=0x00000000000000000000000000000000000000aa
curl -sk -X POST $API/v1/dev/deposit -H 'content-type: application/json' \
  -d "{\"account\":\"$ACCOUNT\",\"amount\":\"50000\"}"
PRICE=$(curl -sk $API/v1/markets | jq -r '.[] | select(.symbol == "BTC-USD") | .mark')
curl -sk -X POST $API/v1/orders -H 'content-type: application/json' \
  -d "{\"account\":\"$ACCOUNT\",\"client_id\":1,\"market\":1,\"side\":\"buy\",\"qty\":\"0.25\",\"kind\":{\"kind\":\"market\",\"quoted\":\"$PRICE\"}}"
curl -sk $API/v1/accounts/$ACCOUNT
curl -sk "$API/v1/accounts/$ACCOUNT/history?limit=10"
```

`-k` only skips the CA check that a self-signed certificate cannot pass. What makes a connection
trustworthy is pinning the attested key. The attestation serves a `bundle` whose SHA-256 is the
first 32 bytes of its `report_data`; the rest of the report data is zero. The bundle is the tag
`engine.attestation.v1`, then the enclave's 20-byte address, the config file's SHA-256, and the
SHA-256 of the certificate's public key (its DER SubjectPublicKeyInfo), the form
`curl --pinnedpubkey` checks. Check that the bundle matches the report data before pinning its key,
and against a real enclave, verify the quote first:

```bash
ATT=$(curl -sk $API/v1/attestation)
BUNDLE=$(jq -r '.bundle[2:]' <<< "$ATT")
EXPECTED="$(xxd -r -p <<< "$BUNDLE" | sha256sum | cut -c1-64)$(printf %064d 0)"
if [ "$EXPECTED" = "$(jq -r '.report_data[2:]' <<< "$ATT")" ]; then
  PIN=${BUNDLE: -64}
  curl -s --insecure --pinnedpubkey "sha256//$(xxd -r -p <<< "$PIN" | base64)" $API/v1/health
else
  unset PIN
  echo "the bundle does not match the report data" >&2
fi
```

The node's metrics are at `localhost:9464/metrics` in Prometheus format, and
[`ops/grafana/engine-node.json`](ops/grafana/engine-node.json) is a Grafana dashboard for them;
[`ops/prometheus/engine-node-rules.yml`](ops/prometheus/engine-node-rules.yml) holds alerting rules
for a node that goes silent or cannot deliver its pages. To have Critical alerts posted to a
webhook, add an `[alerts.critical]` section naming the environment variable that holds its URL, as
`config/dev.toml` shows.

Stopping the node with ctrl-c lets it finish what is queued and write a final checkpoint. You can
then verify the log it wrote:

```bash
cargo run -p engine-node -- replay --log data/dev/log
```

The local configuration accepts unsigned requests (`auth = "none"`). With `auth = "eip712"`, each
order, cancel, and withdrawal must carry its account's EIP-712 signature, its next nonce, and an
expiry at most five minutes ahead, and replay checks every one. The first start records this
setting in the log, so changing it later needs a fresh log: with the node stopped, remove
`data/dev/log`, and the node rebuilds the account history index in `data/dev/history` from the new
one. The node's full API is described in [`crates/node/README.md`](crates/node/README.md).

As the operator, `engine admin` signs a command and submits it over TLS pinned to the node's
attested key, the `PIN` the bundle check above sets. The local configuration names the public key
of a test key, so on a local node this stops new risk in every market:

```bash
OPERATOR_KEY=0x4242424242424242424242424242424242424242424242424242424242424242 \
  cargo run -p engine-node -- admin --pin $PIN set-reduce-only true
```

## Repository layout

The workspace has two crates. [`crates/core`](crates/core) (`engine-core`) is the deterministic
state machine: it performs no input or output, and uses neither clocks, randomness, nor
floating-point arithmetic. [`crates/node`](crates/node) (`engine-node`, binary `engine`) runs it,
sequencing commands, writing and streaming the log, signing checkpoints, serving the API, and
connecting to custody and price feeds. Node configuration lives in [`config`](config), operator
assets such as the Grafana dashboard in [`ops`](ops), the confidential VM image is built from
[`image`](image), and each crate's README describes its files.
[`contracts`](contracts) is a Foundry project holding the custody vault, and
[`scripts/dev-vault-deposits.sh`](scripts/dev-vault-deposits.sh) runs a node against it on a local
Anvil chain through three local RPC proxies. `RPC_QUORUM_SCENARIO=1` adds fabricated-deposit,
provider-outage and quorum-recovery checks; production requires independent provider operators.

## Contributing

Work is tracked in GitHub issues, and pull requests against `main` are welcome. CI checks
formatting, file length, lints, tests, and dependency advisories and licences, replays the
committed reference logs on x86_64 and aarch64 Linux, checks the contracts' formatting, build, and
tests and the vault deposit scenario, and builds the enclave image twice to check that it
reproduces. Run the same checks locally before opening a pull request; the image build is slow, and
matters when the change touches the node or `image`. The code's conventions are documented in
[`agents/AGENTS.md`](agents/AGENTS.md).

## Security

Please report vulnerabilities privately to william.jevons@proton.me rather than in a public issue.
The [security policy](SECURITY.md) explains what a report should include, how quickly to expect a
response, how disclosure is coordinated, and which components are in scope. A bug bounty will be
announced before launch.

## License

monkeytrading is licensed under the Apache License, Version 2.0; see [`LICENSE`](LICENSE).
