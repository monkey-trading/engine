# Development checklist

Load at the start of every coding task, before the first edit, and keep it applied until the work
is done; load it for every code review.

**Rule**: It lists what a change must not miss, and it governs every stage — not a final gate.

## How to use it

- **Before the first edit:** read the applicable sections; they shape the design and the plan.
  Design comes first.
- **While writing code:** apply each applicable item as you write. Concurrency, crash recovery,
  error handling, observability, and tests are built in from the start, never bolted on at the end.
- **Before calling the work done or opening the PR:** self-review the final diff against every
  applicable section and fix what it finds.
- **Reviewing someone else's change:** walk the same sections over their diff.
- A section applies when the change touches what its **When** line names; sections without one
  always apply.
- Each item is a question. An unmet item the code, config, or tests can settle is a defect: fix it
  while developing, report it while reviewing. An item they cannot settle is stated as unverified,
  never assumed.
- Weigh each item against what the code is for and where it runs. A dev-only mock, a test harness,
  or a service that runs as one replica by design does not owe production hardening: note an item
  that does not apply, with the reason, rather than adding machinery for it. Every guard you add
  names the concrete scenario in this deployment that needs it.

## 1. Intent and scope

- Does the change do what its issue or task asks, and is anything it requires missing?
- For a feature: is it complete — every acceptance criterion met, the feature built to run end to
  end on a dev node, and its dev scenario's check passing — and does any part of its design still wait on
  an open decision
  ([agent-plan-feature](../skills/agent-plan-feature/SKILL.md))?
- Does the change carry edits unrelated to that intent?
- Is the solution sized to the problem — no speculative abstraction, configurability, or
  abstraction with one caller ([minimal-change](minimal-change.md))?

## 2. Design and data flow

When: calculations, persistence, service or module boundaries, new or updated dependencies.

- Which service or component owns the calculation and the authoritative data? Is there one
  source of truth, or can two derivations drift?
- Trace the data flow end to end: where it enters, where it is persisted, what is cached, and how
  the cache is invalidated.
- What new dependencies — services, crates, infrastructure — does it introduce, and what happens
  if each fails at the worst time?
- Is each new library needed — nothing in the workspace or standard library already does it — and
  within the dependency rules of the language's `*-implementation` skill?
- Is each new dependency from its official registry or upstream repository, with its exact name and
  publisher verified against typosquatting?
- Does each dependency bump link its changelog or release notes and confirm that the resulting
  behavior is expected?
- How do scheduling, retries, persistence, and recovery interact?
- Does the responsibility sit in the right service or module, with the smallest public surface?
- In the code the change touches, does each layer do only its own job? Protocol rules live in
  `engine-core` and are testable without IO; the sequencer stamps, logs, and applies commands and
  makes no protocol decisions; API, feed, and custody handlers translate into commands and
  delegate.
- Is it consistent with similar implementations in the repository, and is any departure
  justified?

## 3. Concurrency

When: async tasks, threads, multiple replicas, shared state, writes from more than one place,
retries, queues.

- Is in-process state owned by one task and changed only through messages sent to it? That is the
  repo's convention — the sequencer owns the engine, and reads go through it as queries; shared
  state behind a lock needs a stated reason the single-owner design does not fit.
- Can two tasks, threads, or replicas act on the same state at once? What serializes them — a
  single owner, a unique constraint, a transaction, a row lock, or an advisory lock?
- Is every read-modify-write atomic (compare-and-set, conditional `UPDATE`, `SELECT … FOR
  UPDATE`), and is a failed compare-and-set handled, never swallowed?
- Is every retried or redelivered operation idempotent — an idempotency key, a unique constraint,
  or an upsert?
- Does ordering matter, and is it guaranteed or enforced?
- Are locks held for the shortest scope — no await while holding one, no lock-order deadlock?
- Is async code cancellation-safe: can a dropped future (timeout, `select!`, shutdown) leave
  state half-updated?
- Does it still hold with the service scaled to more replicas?

## 4. Crash recovery and restart

When: stateful services, background jobs, multi-step operations, in-memory state, external side
effects.

- If the process dies at any line — between two writes, or after an external call but before
  recording it — what state is left, and does the next start repair it?
- Is a crash logged, and does it stop the service? A background worker, task, or thread that dies
  must stop or restart the whole service — something awaits or supervises it — so the service
  never runs on without that loop while looking healthy.
- Does anything that must survive a restart live only in memory: counters, nonces, queues, or a
  cache that is the source of truth?
- On startup, does the service reconcile in-flight work from persisted state (and from the chain
  or external system) instead of starting blank or repeating it?
- Is each side effect recorded before, or atomically with, its trigger, so a restart neither loses
  nor duplicates it?
- Can work stuck in an intermediate state be detected and resumed or failed, with a timeout?
- Does shutdown drain or checkpoint in-flight work on `SIGTERM`, and does the service come back
  from a kill or restart without an operator when the state it finds is consistent?
- An enclave restart replays the log from host storage it cannot trust, and generates a new
  signing key unless the key is sealed or released on attestation. Does the change hold under
  both: a truncated or altered log, and a different signer for later checkpoints?
- When the state it finds is inconsistent — persisted data that contradicts itself or the chain —
  does it raise a Critical alert and freeze without changing anything, or fail closed where
  continuing could act on diverged books, for an operator to repair? It never guesses a repair.

## 5. Persistence, rollout, and compatibility

When: the event log, checkpoints, stored formats, config, public APIs, stream messages, contracts,
deploy.

- Is every command durable in the log before a client, the stream, or custody sees its effect?
- Is a deploy ordering required — node against vault contract, config against code — and is it
  stated in the PR?
- Is rollback safe, including for log entries and checkpoints the new version already wrote?
- Do REST, WebSocket stream, event, and log-format changes stay compatible for existing
  consumers, including third parties replaying the public log?
- Are new config values and secrets set for every environment, with safe defaults?

## 6. Determinism and replay

When: `crates/core`, anything that builds `Sequenced` commands, the state root, or genesis.

- Does `engine-core` stay free of clocks, randomness, floats, IO, and `HashMap` / `HashSet`
  iteration? Is every order that affects fills, close-outs, or events fixed (`BTreeMap`, sorted
  keys, sequence order)?
- Is every input a decision depends on inside a sequenced command — prices, time, benchmark rates,
  parameters — so a replay needs nothing but the log?
- Does the change alter engine state, its serialization, or what an existing command does? Then
  existing logs no longer replay to their roots: is there a stated plan (rules switched at a
  recorded seq, or a new genesis), and a test that a log from the old code still verifies?
- Is valid input handled without a panic? A panic in the engine halts the protocol and repeats on
  every replay; only internal invariants may assert.

## 7. Money and precision

When: amounts, prices, rates, fees, balances, share calculations.

- Are values decimal or integer units, never binary floats?
- Is every rounding mode and direction explicit and in the protocol's favor where it matters?
  `Fixed` multiplication and division truncate toward zero; is that the right direction for this
  fee, spread, or financing amount?
- Are overflow, underflow, and division by zero impossible or handled?
- Are units and decimal places converted consistently at every boundary?

## 8. Chain data

When: RPC reads, events, receipts, transaction submission.

- Are reorgs and confirmation depth handled?
- Does a read across backends compare the same block, and does an absence carry the answering
  node's height before it counts as "not there"?
- Are nonces, gas, and resubmission safe under retries, concurrent senders, and restarts?

## 9. Errors

- Is every error handled or propagated with context, never swallowed?
- Does every external call have a timeout, and a bounded retry?
- Is each new failure visible: a log at the right level, a metric, and an alert where an operator
  must act?

## 10. Observability

When: new or changed logging, units of work, task or service boundaries, alerts.

- Does each unit of work — a request, a message, a job run, an operation — get one correlation
  scope carrying its id, so every line beneath it carries that id? Long-running loops get none.
- Does the id follow the work across every boundary — into spawned tasks, through queues and
  messages, and into calls to other services? A unit of work started from another records the
  originating id in a role field, so the chain stays joinable.
- Is every id in its own field — the subject's under `id`, other entities under role names
  (`order_id`, `account`, `seq`) — and never only inside a message string?
- Are levels right: `info` for a state transition of an identifiable thing, `debug` for per-step
  detail and "why nothing happened", `trace` for mechanical echoes with no decision?
- Can a failure be diagnosed from the logs alone? Following one id shows every step the unit took —
  its inputs, each decision and its reason, the external calls and their results.
- Is each alert fired inside the unit's scope, so it carries the same ids as the logs? Is its
  severity right — Critical only when the system is blocked and an operator must act now, Warning
  for anything that can wait — and does it fire once per episode rather than on every check?
- Are logs free of secrets and personal data?

## 11. Security and trust boundaries

When: user, API, chain, or config input; auth; secrets; crypto; external calls.

- Is authorization checked where the action happens, not only upstream?
- Is input validated at the boundary it crosses, and when a check fails, does the code refuse to
  act — alert, log, change nothing — rather than "recover" on data it just proved wrong?
- Are secrets kept out of code, logs, state, and committed files?
- Is all crypto from vetted libraries, never home-rolled?
- Is anything the enclave's host supplies — clock, disk, network, the log it replays — trusted
  without a stated bound or check?
- Does everything the enclave key signs carry a domain tag, the chain id and contract where it is
  used, and a nonce, so a signature cannot be replayed in another context?

## 12. Performance and resources

When: hot paths, loops over external data, queries, caches, queues.

- Any unbounded loop, query, memory growth, or queue?
- Any N+1 query or missing index for a new query shape?
- Any blocking call on an async path?
- Is the work per sequenced command bounded? A scan over every account, or a full-state hash,
  on the sequencer's path needs a stated limit or a plan to remove it.

## 13. Tests

- **They test what they claim.** A test's name matches its assertions, and each assertion would
  fail if the behavior broke. No tautologies, no asserting that a mock returned what it was told,
  no bare `is_ok()` or no-panic check where the value matters.
- **Mutation-checked when the tests run anyway.** For a new or changed test, make one temporary
  edit to the code under test that breaks exactly what the test's name claims, and run it: that
  test must fail and nothing unrelated should. Undo the edit by hand, never with `git checkout` or
  `git restore`, which also discard uncommitted work. A test that stays green is a defect. Without
  a test run, ask: if this line were inverted, which assertion would fail?
- **They cover the changed branches.** The success path, every new or changed error path,
  boundaries (zero, empty, maximum, off-by-one), missing or `None` inputs, retries and timeouts,
  and every failure mode sections 2–12 found — including concurrent callers and a restart from
  persisted state where the behavior is testable.
- **Right level.** Unit tests for logic; integration tests where the database, network, or chain
  boundary is the behavior. End-to-end coverage is the operator's call.
- **Deterministic.** No sleeps for synchronization; time, randomness, and network are controlled;
  no dependence on test order or on shared global state.
- **Regression.** A bug fix carries a test that fails without the fix.
- **Not weakened.** A deleted, skipped, or ignored test, a loosened assertion, or a widened
  tolerance is a defect unless the PR justifies it.
- **Realistic data.** Amounts use production precision; fixtures match real shapes.
- **Actually runs.** The tests sit in a CI lane that runs for these paths.

## 14. Code quality and domain skills

- Does the change follow every `*-implementation` skill for the files it touches — these, and any
  other the [AGENTS.md](../AGENTS.md) index lists?
  - [rust-implementation](../skills/rust-implementation/SKILL.md) — the Rust workspace.
  - [smart-contract-solidity-implementation](../skills/smart-contract-solidity-implementation/SKILL.md)
    — Solidity contracts.
- Does it add process-global mutable state — statics, singletons, a library's default registry?
  That is a hidden dependency: it breaks test isolation and allows only one instance per process.
  The language's `*-implementation` skill lists the exceptions.
- Clean and idiomatic, with no dead code, unused dependencies, or leftover debugging the change
  created.
- Names come from the design or issue vocabulary, spelled out (`client_qty`, not `figures` or
  `data`). A constant or variable whose name no longer describes its use gets a new name, not
  a new use.
- Comments are terse and describe current behavior ([code-comments](code-comments.md)).

## 15. Docs and indexes

- Are the nearest `AGENTS.md`, README, and runbooks updated for changed commands, paths, env vars,
  or behavior ([keep-agents-index-in-sync](keep-agents-index-in-sync.md))? A README passage is
  rewritten as the rule in force, never extended with a "from V<n>" paragraph, by
  [readme-writing](../skills/readme-writing/SKILL.md).

## 16. Workspace rules

- Does the change follow every rule and skill [AGENTS.md](../AGENTS.md) selects for it?
