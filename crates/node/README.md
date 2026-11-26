# engine-node

The `engine` binary, which runs the trading engine inside the enclave, publishes its log and serves
the API.

`engine-node` builds the `engine` binary. Run as a server, it puts every incoming command in order,
writes the command to a public, append-only log, and then applies it to `engine-core`. A command's
effect reaches no client until the command is synced to disk. The server signs checkpoints of the
state root with a key that only its enclave holds, serves the REST API and the WebSocket stream,
reads deposits from the custody vault, and takes prices from the venues' feeds.

The host machine controls the enclave's clock, disk and network, so the server trusts none of them
without a check. It bounds the host's clock with signed Roughtime readings, refuses a log that
does not match the checkpoints the vault holds, and terminates TLS inside the enclave. The same
binary also replays a published log, verifies attestations and signed answers, and provides the
operator's signing tools.

## Usage

| Command | Purpose |
| --- | --- |
| `engine run` | Runs the trading server. `--new-key` seals new keys for a release that has none, `--predecessor <host:port>` takes the keys from the running release once the vault makes this release current, `--standby` follows the operator in effect as a standby copy, and `--frozen` refuses every client request, for a log's last starts before a new log carries its state, and the new log's starts before its public opening. |
| `engine replay` | Replays a log directory, a downloaded archive, a single log file, the archive at a trading server's URL, which it downloads through the server and the bucket the server redirects to, or a log's place in the archive's bucket, which it reads from the bucket alone through the vault's checkpoint that `--seq` and `--state-root` name, and checks every state root and checkpoint signature. With `--attestation`, it also requires every checkpoint signer to be a verified enclave. It checks every instruction's chained digest and every seal's form, and in full the first and last seal of each run of one signer's seals, the last authenticating every instruction in the run. `--every-seal` checks every seal in full, on one thread per core or on the number `--replay-workers` names. `--snapshots <dir>` replays a log directory in ranges, several at once, from a directory of the daily snapshots a trading server with `[daily_snapshots]` serves at `/v1/archive/snapshots/`, or from the server's URL, downloading each snapshot it lists through the server and the bucket the server redirects to, each checked against its checkpoint's signed root. `--carry-source <log>` also checks a log whose genesis carried another log's state against that log's signed checkpoint. |
| `engine carry` | Builds, from a published log's checkpoint, the carry file a new log's genesis takes each account's MONKEY entitlement and stake and every bug bounty award from, and prints what it carries. It takes the log as a directory or as a trading server's URL. |
| `engine exit-proof` | Builds, from a published log alone, the proofs and vault calls with which an account recovers its funds through the vault's exit. The log can be a directory, a trading server's URL, or the archive's bucket alone, read through the vault's latest checkpoint, `--seq` and `--state-root`. |
| `engine verify-attestation` | Checks an enclave's attestation against a release manifest. |
| `engine verify-response`, `engine verify-frames` | Check a saved signed answer, or a saved signed stream connection, against the attested key. |
| `engine admin` | The operator's client. It signs administrative instructions with the operator's key and submits them over TLS pinned to the attested key, and it delivers secrets into the enclave. |
| `engine ct-watch` | Watches Certificate Transparency logs for a certificate for the API's domain whose key no verified attestation names. It runs outside the enclave. |

`engine <command> --help` lists each command's options.

To run a local server with a simulated enclave, custody and prices, run the following from the
repository root:

```shell
cargo run -p engine-node -- run --config config/dev.toml
```

The server serves the API at `https://127.0.0.1:8080` with a self-signed certificate, and metrics
at `http://127.0.0.1:9464/metrics`. It writes the log to `data/dev/log`. `node.listen`,
`metrics.listen` and `node.log_dir` in the configuration file change these. Configuration keys
and commands call a running trading server a node, as these keys do. The server processes
nothing until two of three public Roughtime servers agree on the time, and then logs
`first trusted time sample: sequencing started`. The [root README](../../README.md#running-it)
walks through this case and the others.

The metrics include three latency histograms an operator watches under load. The first is the time
a venue quote waits from its receipt to its stamp, `engine_price_to_stamp_seconds`. The second is
the time from the API admitting a signed request to its reply, `engine_api_request_seconds`. The
third is the time from a command's stamp to its send to each stream subscriber,
`engine_stream_lag_seconds`. The counter `engine_sequencer_busy_seconds_total` grows by the time the
sequencer's thread spends working, so its rate is the share of the time the thread is busy. The
Grafana dashboard in [`ops/grafana/`](../../ops/grafana/) shows all four.

The server provides a REST API under `/v1` and a WebSocket stream at `/v1/stream`. The
[API reference](../../docs/api.md) lists every endpoint, request body and answer, and describes how
requests are signed and how a client verifies the server's signed answers.

## Layout

| Area | Modules | Responsibility |
| --- | --- | --- |
| Startup | `node/`, `config/`, `telemetry`, `sequencer_thread` | Loading and checking the configuration, starting each part of the server in order, and the sequencer's dedicated thread |
| Sequencing and the log | `sequencer/`, `root_worker/`, `log/`, `archive/`, `snapshot/`, `daily_snapshots/`, `authenticated/`, `replay_audit/` | Putting commands in order and applying them, the state root's worker thread, the segmented public log and its archive in object storage, recovery, snapshots, the daily snapshots and their archive, and replay |
| API | `api/`, `tls/`, `limits/`, `access/`, `withdrawals/` | The REST API and stream, TLS with a certificate from an ACME authority, rate limits, account screening and withdrawal authorisations |
| Reads | `history/`, `stats/`, `status` | The account history index, the price history cache, the dashboard statistics and the operating status |
| Enclave and time | `tee/`, `secrets/`, `handover/`, `clock`, `trusted_time/`, `roughtime/`, `verify/` | Attestation, sealed keys, delivered secrets, handing the keys to the next release, trusted time and attestation checks |
| Custody and the chain | `custody/`, `checkpoints/`, `log_heads/`, `log_check/`, `exit/`, `carry/`, `seed_relay/` | Deposits and the RPC quorum, checkpoint posting and reconciliation, on-chain log heads, the log check at each turn, exit proofs, the carried genesis at a move to a new vault, and the turn-seed relayer |
| Operator rotation | `standby/`, `standby_key/`, `liveness/` | Following the operator in effect, standby keys, the liveness rule that promotes a standby, and fixing the fallback seed of a turn that no relay seeded |
| Prices | `feeds/`, `benchmarks/` | Venue and market-data feeds, and the fetches behind dated benchmark rates |
| Operations | `metrics/`, `monitor/`, `storage/`, `alerts/`, `episodes`, `throttle`, `admin/`, `ct_watch/` | Prometheus metrics, self-monitoring, disk alerts, alert routing, `engine admin` and `engine ct-watch` |

Each module's own documentation describes how it works. The
[runbooks](../../ops/runbooks/README.md) hold the operator's procedures, and
[`ops/runbooks/alerts.md`](../../ops/runbooks/alerts.md) lists every alert the server raises.

## Configuration

The server reads one TOML file, named by `--config` or `CONFIG_PATH`, with `config/dev.toml` as the
default. Every section refuses keys it does not know.

| Sections | What they configure |
| --- | --- |
| `[node]`, `[logging]` | The API's address, the log directory and segment size, the checkpoint interval, client authentication, the threads that check instruction seals in a replay, and the log level |
| `[api]` | Connection and request limits, timeouts, browser origins, the PROXY protocol and the ACME certificate |
| `[tee]`, `[operator]` | Attestation, the CPU models a peer enclave's quote may come from, key sealing, the handover and secrets listeners, and the operator's public key |
| `[custody]`, `[checkpoints]`, `[log_heads]` | The vault, its RPC providers, the operator registry's contracts, checkpoint posting and log head posting |
| `[liveness]`, `[seed_relay]`, `[standby]` | A rotation member's liveness caller, the turn-seed relayer and standby mode |
| `[feeds]`, `[time]`, `[benchmarks]`, `[access]` | Price feeds, Roughtime servers, benchmark publishers and the screening source |
| `[history]`, `[stats]`, `[snapshots]`, `[daily_snapshots]`, `[archive]` | The history index and price history cache, dashboard retention, recovery snapshots, the daily snapshots and how long the host keeps them, and the bucket that keeps the log's archive and every daily snapshot |
| `[metrics]`, `[alerts]` | The metrics listener, alert routing to webhooks and the disk alerts' thresholds (`[alerts.storage]`) |
| `[genesis]` | The parameters and markets that are sequenced into a new log on its first start, the offset groups of portfolio margin among the parameters, and in `[genesis.carry]` the log whose MONKEY, stakes and awards it carries |

[`config/dev.toml`](../../config/dev.toml) runs everything against local simulations.
[`config/anvil.toml`](../../config/anvil.toml) runs custody against the vault on a local Anvil
chain. A production configuration is a deployment's node sections, which start from
[`ops/runbooks/production-node.toml`](../../ops/runbooks/production-node.toml), followed by
[`config/benchmark-publishers.toml`](../../config/benchmark-publishers.toml) and
[`config/production-genesis.toml`](../../config/production-genesis.toml). A release ships one for
production and one for staging, from [`config/release/`](../../config/release).

The `[genesis]` section is part of the protocol. A log records the genesis it started with, and the
server refuses to resume a log whose genesis differs from the configuration's. If you change
`[genesis]`, start with an empty log directory.

`engine admin` reads the operator's key from `--key-file`, or else from the file that
`OPERATOR_KEY_FILE` names, or else from `OPERATOR_KEY`. In a development build, a webhook URL or a
vendor's API key may come from the environment variable that the configuration names with `url_env`
or `api_key_env`. A production build refuses both and takes those values only as secrets delivered
into the enclave. The one exception is a standby, which runs outside any enclave.

The crate has two features. A `production` build refuses a simulated enclave, custody or price
feed, an API without an ACME certificate, a genesis market that fewer venues quote than it requires,
and a secret read from the environment. The release image is built with this feature.
`short-turns` makes an operator's turn five minutes long for development stacks, and a `production`
build does not compile with it.

## Testing

```shell
cargo test -p engine-node
cargo test -p engine-node --features production --lib production_build
```

The first command runs the unit tests and the integration tests in `tests/`, which replay the
crate's reference logs and check the production configuration templates. Two of those test files run
Python scripts, and need Python 3.11 or later. The second runs the tests
that hold only for a production build, which refuses the development configuration that the other
tests load.

The `scripts/dev-*.sh` scenarios run the server end to end against local stand-ins for the chain,
the venues and the market-data vendors. Each script's header says what it does and what it needs.
The benchmarks of the sequencer's latency at 100,000 accounts are ignored by default and need a
quiet machine. `sequencer::wait_bench` and `stats::launch_bench` give their commands in their
module documentation.

The load test runs the server on a launch-scale book of 100,000 accounts and loads it through its
API, its stream and its feeds. Two thousand traders behind 100 client addresses send signed orders,
cancels, cancel-alls and close-alls, every stream subscriber the API allows reads the stream, and
loopback stand-ins quote every launch market. It runs at launch flow and at ten times launch flow,
two minutes each, and prints each run's latencies and the sequencer's busy share against the launch
bar: the budgets for the time from an order to its fill, from a price to its stamp and from a stamp
to each subscriber, and for the share of the time the sequencer is busy. A run fails when a budget
breaks. It also prints the CPU and memory of the test's process, which holds the server, the
stand-ins and the screening source. The clients run in a second process. The `--ramp` option then
doubles the load each minute and prints the load at which each budget first breaks. At full size the
test takes about 4 GB of memory and about five cores, so run it in a quiet window:

```shell
nice ./scripts/dev-load.sh
```

The script's header lists the variables that size a smaller run.

The recovery test writes a long log of launch flow on the same book through the server's sequencer,
on a clock that skips ahead to each instruction, then times each way the server comes back on it: a
full replay, a restart from a cache, a rotation member's replica build, and turn handovers with tails
of up to about 512 MiB. It prints a full replay of 30 days of launch flow, projected from the replay's
rate over the launch flow, against 30 minutes, a restart from the cache against 2 minutes, and each handover's switch
against 1 second. A day of launch flow writes several gigabytes of log, so size the run to the disk you
have, as the script's header and the `sequencer::recovery_scale` module documentation describe:

```shell
RECOVERY_LAUNCH_SECS=3600 nice ./scripts/dev-recovery-scale.sh
```

The leg of ten times launch flow that follows the launch leg is a tenth as long, unless
`RECOVERY_TEN_TIMES_SECS` names it, so the command above writes about an hour and six minutes of log.

## License

Licensed under the Apache License, Version 2.0, whose text is in [LICENSE](../../LICENSE).
