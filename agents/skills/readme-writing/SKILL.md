---
name: readme-writing
description: >
  Use before writing, rewriting, or editing any README.md or any other document a person outside
  the project reads: SECURITY.md, pages under docs/, operator runbooks under ops/runbooks/, and
  research write-ups. Covers who each is for, the order of a README's sections, what belongs in one
  and what never does (version history, file inventories, issue numbers, agent workflow), the root
  and crate templates, the security policy and runbook patterns, where moved text goes, how a long
  page opens, how to state status, limits and warnings, the prose and command rules, and the check
  before a document lands. Modelled on go-ethereum's README. Overrides any older README template in
  another skill.
---

# Writing a README

A README is the front door. It answers, in order, the questions of a stranger who has just
landed on the repository: what is this, how do I get it, what do I get, how do I run it for my
case, how do I talk to it, how do I help, and under what licence. It answers them for the code as
it is today, in as few words as answer them, and links out for everything else.

The model is [go-ethereum's README](https://github.com/ethereum/go-ethereum#readme). Read it before
writing one. Note what it does:

- **One line of identity** under the title: "Golang execution layer implementation of the Ethereum
  protocol." No adjectives, no pitch.
- **Building** names the prerequisites with versions (Go 1.25 or later, a C compiler) and gives
  the one command, then the second one.
- **Executables** is one table: each binary and what it is for, with an example invocation.
- **Running** opens with concrete hardware requirements, then walks the common cases in order of
  how many people have them: a full node on mainnet, then on a test network, then configuration,
  Docker, the RPC interfaces, and a private network. Each case states the reader's goal, gives the
  exact command, then says what the command does and which defaults it sets.
- **Warnings** appear once, in bold, at the point where the reader could hurt themselves (exposing
  RPC to the internet), and say what the danger is.
- **Depth is linked, not copied:** the full flag list, the developer guide and the network setup
  each live on their own page.
- **Contribution** is short: fork, fix, open a PR against the main branch, talk to the maintainers
  first for large changes, follow the formatting and doc conventions, prefix commits with the
  packages they touch, with an example.
- **License** says exactly which code is under which licence and which file holds it.

## Who reads it

Write for two readers, in this order:

1. **Someone deciding whether this is for them.** They read the title line and the first
   paragraph, and maybe the status. Everything they need to decide is there.
2. **Someone trying to use it.** They skim headings to find their case, copy a command, and run
   it. Everything they need for their case is under one heading, in the order they will do it.

A maintainer who already knows the code is not the audience. Their reference is the code, its
module docs, `agents/AGENTS.md`, and the runbooks.

## What goes in a root README

In this order, leaving out a section that has nothing true to say:

1. **Title and one line.** The project's name and one sentence saying what it is, in the reader's
   terms. Then, at most, a short paragraph on what makes it different, stated as facts the code
   makes true.
2. **Status.** One short paragraph or a small table: what runs for real, what runs against local
   stand-ins, what is not built yet, and whether it holds real funds. Present tense, no dates of
   past work, no list of everything that ever landed.
3. **How it works.** The model a user needs to predict the system's behaviour, in plain words: what
   happens to an order, what it costs, when it can be refused, and what the user can check. Use a
   table where the reader compares values (fees by tier, margin by band). Describe the rules in
   force today. Link the specification or the code for the detail.
4. **Getting it.** Prerequisites with exact versions, then the build command. If there are
   releases or images, where they are.
5. **What you get.** A table of the binaries, packages or services, one line each with an example
   invocation, like go-ethereum's Executables table. A project with one binary and many
   subcommands tables the subcommands, then lists the other artefacts (contracts, a browser
   package, an image) in a second short table.
6. **Running it.** The common cases, most common first, each under its own heading:
   - the reader's goal, in a sentence;
   - the exact command, in a fenced `shell` block that copies and runs from a fresh checkout;
   - what the command does and the defaults it sets, in a few bullets (ports, data directory,
     which parts are mocked);
   - how to tell it worked.
7. **Talking to it.** The interfaces a program uses (API, stream, CLI), with the options that turn
   them on and their defaults, and a link to the full reference.
8. **Verifying it**, where the project makes trust claims: the exact commands a reader runs to check
   them, and what each check proves.
9. **Contributing.** How to send a change, the coding conventions in a few bullets, the commit and
   PR title format with an example, what CI checks, that a maintainer approves each PR before it
   merges, and where to ask first for a large change.
10. **Security.** One line pointing at `SECURITY.md` and how to report privately.
11. **License.** Which code is under which licence, and the file that holds each licence's text.

Target length for a root README: what the sections above need, usually 150 to 300 lines. A README
that passes 400 lines is carrying something that belongs in a linked document.

When the rules a user meets need more than about 100 lines (a trading product's fees, tiers,
margins and close-outs, say), "How it works" keeps the model in a few paragraphs with the one or
two tables a newcomer needs, and the full rules go in a linked page under `docs/`, written to this
skill's standard. The README stays the front door. Formal sentences run longer than compressed
ones; a README a little over 300 lines in that register is better than a shorter one in shorthand.

## What goes in a crate, package or directory README

A crate README tells someone opening that directory what the crate is for and how to use and test
it. In this order:

1. **Title and one line:** the crate's name and what it does.
2. **What it is for:** a short paragraph: its job, what depends on it, and the one or two
   properties it guarantees (for example, deterministic, no IO, no floats).
3. **`## Usage`:** the entry points a caller uses (types, functions, binaries, endpoints), with a
   short example.
4. **`## Layout`:** at most a table of its top-level modules or directories, one line each, saying
   what each one is responsible for. Never one row per file.
5. **`## Configuration`:** environment variables or config sections it reads, with defaults. Leave
   the section out if there are none.
6. **`## Testing`:** the commands that test it, and any that need extra tools or a quiet machine.
7. **`## License`:** one sentence that names the licence and links the file holding its text, such
   as `Licensed under the Apache License, Version 2.0, whose text is in [LICENSE](../../LICENSE).`
   Which code is under which licence is for the root README to say.

Every crate README uses these headings, spelled as written and in this order, so that a link or a
later change finds the same section in each crate. The title line and the paragraph on what the
crate is for sit under the title, with no heading of their own. A section only maintainers need,
such as how to add a protocol version, goes after `## Testing` and before `## License`.

Target length: 40 to 150 lines. A crate whose README needs more is documenting its internals,
which belong next to the code ([Where moved text goes](#where-moved-text-goes)). The README of an
operational directory, such as an image or release directory, or a runbooks index, may run past
150 lines when its procedure is the content. The steps an operator follows in order stay together
on one page.

## What never goes in a README

- **History.** No "from V59", "now", "no longer", "previously", "was changed to", "since #123".
  A README states the rules in force. Version history lives in the code's version list, the
  changelog or release notes, and git. Where a reader must know that behaviour differs by version
  (replaying an old log), one sentence says so and links the version list.
- **Accretion.** A change never appends a sentence describing itself. It rewrites the passage it
  affects so the passage reads as if written today, from scratch, and deletes what is no longer
  true.
- **Inventories.** No list of every file, every test, every script, every alert or every config
  key. Name the few the reader needs and link the directory or the reference that has the rest.
- **Internal references.** No issue numbers, decision ids (D5, #614), PR numbers, branch names,
  review rounds, finding ids or audit labels. If a fact matters, state the fact.
- **How the project is run.** No waves, milestones, agent workflow, review loops or session notes.
  The few rules a contributor meets (CI must pass, a maintainer approves the merge) go in the
  contribution section in one sentence each; everything else stays in `agents/`.
- **Unbacked claims.** No "fast", "secure", "robust", "seamless", "battle-tested",
  "production-grade", "comprehensive", "powerful" or "simply". State the number or the mechanism,
  or leave it out.
- **Other organisations' names**, competitors, and reference products. Vendors the code actually
  uses are named where the reader needs them.
- **Legal analysis**, of any kind.
- **Decoration.** No emoji, no badge that is not true and useful (CI status, docs and chat are
  fine), no table of contents for a short README, no heading over a single sentence.

## Where moved text goes

A rewrite that takes text out of a README moves each true fact to where its reader looks for it:

- **Rust internals** go to the `//!` documentation of the module that implements them.
- **Contract internals** go to the contract's Solidity NatSpec.
- **Operator procedure** goes to the runbook under `ops/runbooks/` that covers it.
- **Client-facing reference,** such as the API's routes or the rules a trader meets, goes to a
  page under `docs/`.

Text that becomes a comment follows [code-comments](../../rules/code-comments.md): it is cut to the
sentences the code needs, not pasted whole.

A page under `docs/` is written to this skill's prose standard. A page longer than about 300 lines
opens, after its first paragraph, with a list of links to its `##` sections, so that a reader can
go straight to their case. A shorter page has no such list, as a short README has none.

## Other public documents

Every document a person outside the project reads is held to this skill's prose, sentence and
command rules, not only READMEs. That covers `SECURITY.md`, every page under `docs/`, the runbooks
under `ops/runbooks/`, and the research write-ups. A document that has grown by appending a
paragraph per change is rewritten as a whole, as a README is, with every true fact kept.

### A security policy (`SECURITY.md`)

Its readers are a researcher deciding whether to report something, and an auditor or user deciding
what the system can be trusted for. In this order:

1. **Reporting a vulnerability:** where and how to report privately, what to include, and what not
   to do. Short, at the top.
2. **What happens next:** response times, how a report is handled, and how fixes are disclosed and
   credited.
3. **Safe harbour.**
4. **Scope:** what is covered, as a short list or table of the components, one line each, then the
   findings that matter most, the findings that start from an operator's key, what is out of
   scope, and what counts as a new finding.
5. **Supported versions** and **bug bounty**, short.

The policy stays at about 200 lines. It links, near the top and from the scope, to
`docs/security-model.md`, which holds what each component is trusted for and its known limits:
one `##` heading per component (the engine, the trading server, custody, the enclave, price feeds,
operator rotation, and so on). Under each, first what the component guarantees, then its known
limits as a list. Each limit is one item of one to three sentences: what an attacker or failure
can do, what bounds it, and, where it applies, what a report about it alone needs to add to count
as new. Group limits under the component a reader would look in, never in one long running
section. The model opens with links to its component sections, grouped as a reader would look for
them.

A limit is a commitment, so a rewrite keeps every one, with its exact bound, and never softens or
drops one to shorten the page. A limit is stated as the system as it is today, never as "until",
"from version", or an issue number.

The policy speaks as "we" for the project's commitments: its response times, containment,
disclosure and safe harbour. This is the one exception to the rule against "we" in
[Sentences](#sentences). The security model, every other page under `docs/`, the READMEs and the
console's interface copy keep that rule.

### An operator runbook (`ops/runbooks/`)

Its reader is an operator under pressure, often during an incident. Each runbook, or each
procedure within one, follows the same order:

1. **When to use it:** the alert, symptom or decision that brings the operator here, in one or two
   sentences.
2. **What is happening:** the cause in plain words, and what it affects for clients.
3. **What to do:** numbered steps, one action each, with the exact command in a fenced block and
   the output that shows it worked.
4. **How to tell it is resolved,** and **when to escalate,** and to whom.

Background the operator does not need in order to act goes after the procedure, or in the module
docs. Warnings sit at the step they apply to, in bold.

### Research write-ups (`research/*/README.md`)

The question the study answers and its answer come first, as the result with its numbers. Then the
method, the data and its sources, the limits of the result, and how to re-run it, in that order.

## Prose

Follow [writing-style](../../rules/writing-style.md), plus:

- **Second person and present tense:** "You can run the trading server against a local chain",
  "The engine refuses an order when…".
- **Short paragraphs,** three to five lines. A paragraph that lists more than three things is a
  list.
- **Lists** for flags, options, steps and requirements. **Tables** for anything the reader compares
  across rows. **Prose** for how things work and why.
- **Define a term once,** the first time it appears, in words a trader or developer outside the
  project knows. Prefer the industry's term to the code's when they differ, and give the code's
  name once in backticks if the reader will meet it in output. A README read by traders and
  developers defines each side's terms for the other: basis point, mark, notional and margin for
  developers; enclave, attestation and replay for traders. A term the reader will meet in commands
  or their output (enclave, attestation, checkpoint) is defined once and then used, not replaced
  with a friendlier word.
- **One name for the running service.** The prose of every public page, READMEs and `docs/` alike,
  calls it "the trading server". Commands, config keys and output call it a node, as in
  `engine-node` and `node.listen`. A page that shows them says once, where they first appear, that
  a node is a running trading server, and keeps "node" out of the rest of its prose.
- **Values not yet confirmed** are stated with "proposed" inline, next to the value, never left
  out and never stated as settled.
- **Concrete over general:** versions, sizes, ports, defaults, times, limits, with units.
- **One warning per hazard,** in bold, where the reader would act on it, saying what goes wrong.
  Not a stack of notes at the top.
- **Wrap prose at 100 columns,** one space after a full stop, British or American spelling as the
  repository already uses. Tables, URLs, code spans and link text are never broken across lines;
  let those lines run long. Break a long command with `\` at an argument boundary.

## Sentences

A README is technical documentation for people, read top to bottom by someone who wants to
understand. It is not a dense summary for a machine to parse. Write it the way an experienced
engineer or a broker's documentation team writes: complete, formal sentences that flow, one idea
each, in the standard terms of the field. The console's interface-copy rule sets the same standard
for its help text, and the owner's examples there apply here.

- **Context first, then what happens, then its consequence for the reader.** Each in its own
  clause, usually its own sentence.
- **One condition per clause.** Never stack "while … while … or while …", or "once … then … unless
  …", in one sentence. Two conditions take two sentences, or a short list.
- **No compressed asides.** Write ", spread included,", ", gap included," and "(bps, hundredths of
  a percent)" out as clauses or as their own sentence: "including the spread", "even if the price
  gaps".
- **No colon chains.** A colon introduces a list or a table, not the second half of a compressed
  sentence.
- **No engine shorthand as nouns.** Not "a close", "an add", "a fill opens", "a reversal pays".
  Write "closing a position", "adding to a position", "the part of the trade that opens a
  position", and use the standard term (execution price, realised profit, working orders, margin,
  exposure, close-out level).
- **No comma-chained facts joined by "and … so …".** Three facts are three sentences.
- **Name every subject.** No "this", "it" or "that" for something the sentence has not named.
- **Formal register.** "Does not", "cannot", "you would", never "won't", "can't" or "you'd". Never
  "we" or "I", except for the project's commitments in a security policy
  ([A security policy](#a-security-policy-securitymd)).
- **Read it aloud.** A sentence you would not say to a colleague, or would have to say twice, is
  rewritten.

Before and after, from the first rewrite of this repository's README:

| Before | After |
|---|---|
| A market has no price, and nothing in it fills, while fewer than two venues have quoted in that second, while their quotes spread wider than its divergence bound, in basis points (bps, hundredths of a percent) of the index, or while its exchange is closed. | A market is priced only while at least two of its venues have quoted within the last second and their quotes agree to within the market's divergence limit. When either condition fails, or when the market's exchange is closed, the market has no price and no order in it can be executed. |
| Each closes the whole position once the price you would close it at, spread included, reaches it: a stop loss at that price, gap included, and a take profit at its level or better. | When the price at which the position would close, including the spread, reaches a take profit or stop loss level, the whole position is closed. A take profit executes at its level or better. A stop loss executes at the prevailing price, which may be worse than its level if the market gaps. |
| A losing close pays nothing. A winning close pays a share of the gain it realises, after both fills' spreads, that falls with how long the position was held: | No fee is charged on a losing trade. On a winning trade, a share of the realised profit is charged as a fee, after the spreads paid on opening and closing. The share falls the longer the position has been held: |
| An add moves the open time to the size-weighted average of when the units were added, counting units already held as no more than 15 minutes old, so an add neither restarts the clock nor borrows a small old position's age. | When you add to a position, its opening time becomes the size-weighted average of the times at which each part was added. Any part held for longer than 15 minutes counts as 15 minutes old, so adding to a position neither restarts its holding time nor lets a large new trade benefit from the age of a small old one. |
| The node publishes each command it sequences with the events it produced, and the enclave signs a Merkle root over the whole state at each checkpoint, so anyone can replay the log and check the state they are shown. | The trading server publishes every instruction it processes, together with its effects. At each checkpoint the enclave signs a fingerprint of the entire state, a Merkle root. Anyone can replay the published record and confirm that the state they are shown is the one the rules produce. |

## Commands

- Every command block is copy-pasteable: no prompts, no placeholders the reader cannot fill, no
  elided output. Use `<angle-brackets>` only for values the reader must supply, and say what they
  are right under the block.
- Every command in a README has been run from a fresh clone at the commit that adds it, and its
  stated result observed, unless it needs hardware the writer's machine lacks or hours to run. A
  fresh worktree at that commit counts as a fresh clone. If it needs a tool, the prerequisites list
  it with a version.
- A command that needs such hardware or time still goes in the README, with what it needs and what
  it produces, as building the enclave image and booting it on an SEV-SNP host do.
- Show a command with its real defaults (ports, paths) and name the setting or variable that
  overrides each. Verify it with overrides where a default is in use on your machine; never stop
  or disturb whatever holds the default.
- A README PR's body ends with one line per command it ran and the result. It then names each
  command it did not run, why, and what ran in its place, such as the CI job that builds the image
  on the PR. That list is the evidence this skill's check asks for, and it is the one exception to
  `agent-create-pr`'s rule against a test plan in the body.
- Put the expected result after the command: what prints, which port listens, which file appears.
- Prefer the repository's own scripts and `cargo`, `npm` or `forge` commands to long hand-written
  pipelines.

## Status and limits

State status as a short present-tense table or paragraph:

| Part | State |
|---|---|
| Trading engine | Runs end to end on one machine |
| Custody | Runs against a local chain |
| Hardware attestation | Being built |

Say plainly what holds no real funds, what is mocked, and what is not built. Keep the limits a user
must know (prices can be unavailable, withdrawals queue) in "How it works", not in a disclaimer.

## When to update a README

- A change that alters a command, flag, default, port, path, prerequisite, or user-visible rule
  updates the README passage that states it, in the same PR.
- A change that adds a feature updates "How it works" only if a user's prediction of the system's
  behaviour changes, and then rewrites that passage rather than adding a paragraph.
- A protocol version that changes a rule users meet rewrites the passage stating that rule as the
  rule in force. It never adds a "from V<n>" paragraph.
- A change to internals updates module docs, not the README.

## Check before it lands

- [ ] The first line says what this is, in words a stranger understands.
- [ ] A reader can find their case from the headings alone.
- [ ] Every command was run from a fresh clone and its result is stated, or the PR names why it
      was not run and what ran in its place.
- [ ] No sentence narrates history, a version transition, or how the project is run.
- [ ] No issue, PR, decision or finding number appears.
- [ ] No list of every file, alert, key or test.
- [ ] Every number has a unit, and every claim is a mechanism or a measurement.
- [ ] Each warning is stated once, where it applies.
- [ ] Prose calls the running service the trading server. "Node", for that service, appears only
      where commands, config keys or output use it, and is defined once.
- [ ] Length is within the target, or the excess has moved to a linked document.
- [ ] No sentence stacks conditions, chains facts with "and … so …", or compresses a condition into
      an aside; no engine shorthand used as a noun; no contractions.
- [ ] Read it top to bottom as a stranger. Cut every sentence that reader would skip.
