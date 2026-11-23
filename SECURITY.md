# Security policy

monkeytrading is designed to be trusted without trusting the people who run it. Its rules are public
code, it is built to run inside a confidential-computing enclave, and anyone can replay the
instructions it publishes to check the state it reports. A vulnerability can undermine any of these
properties, so we ask you to report it privately and to give us time to fix it before it becomes
public. This policy explains how to reach us, what you can expect from us in return, and what it
covers.

The [security model](docs/security-model.md) states what each component of the system guarantees,
and its known limits.

## Reporting a vulnerability

Report a vulnerability through GitHub's private vulnerability reporting. Open this repository's
Security tab and choose **Report a vulnerability**. The report is visible only to you and to us. If
you would rather not use GitHub, email william.jevons@proton.me instead. We publish no PGP key.

Do not describe a vulnerability in a public issue, pull request or discussion, even in outline. Keep
the details out of an email's subject line.

A report is easiest to act on when it includes:

- the affected component, and the commit of `main` you tested;
- what an attacker can achieve, and what access they need first;
- how to reproduce it. A failing test, a log of instructions that replays to a different state from
  the one the trading server reported, or a script against a local trading server makes the
  strongest case;
- whether you have a fix in mind, and whether anyone else knows of the issue;
- how you would like to be credited.

## What happens next

We aim to:

- acknowledge your report within three business days;
- tell you, within ten business days of receiving it, whether we can reproduce the issue, how severe
  we judge it, and what we plan to do;
- fix the vulnerability within 90 days of your report, or within 30 days if it is one of the
  [findings that matter most](#findings-that-matter-most);
- keep you informed until the issue is resolved.

If we conclude that a report does not describe a vulnerability, we explain why.

### Containment

Once the engine holds live funds, we contain a confirmed vulnerability according to its severity
before we fix it:

- **Critical:** we pause withdrawals through the vault's guardian, and make every affected market
  reduce-only, so that positions in it can only be reduced or closed. We do both at once.
- **High:** we make the affected markets reduce-only. We also pause withdrawals if funds can leave
  through the flaw.
- **Medium and low:** we contain nothing, and the fix follows at the normal pace.

We keep containment in place until the fix is live. A fix to the engine or the trading server is
live once its release, which the vault's timelock approves, is the vault's current release. A
deployed contract cannot be changed, so a fix to a contract is a new contract. That fix is live once
clients have moved to the new contract. When containment affects users, we announce it at once in
neutral terms, such as "security maintenance", and give the full details when we publish the
advisory.

### Coordinated disclosure

We ask you to keep the details of a vulnerability private until a fix has shipped, or until 90 days
after you reported it, whichever comes first. A fix has shipped when it is on `main`. Once the
engine holds live funds, a fix has shipped only when it is also running in production.

If a fix cannot meet that deadline, we will tell you why before the deadline passes. We will then
agree one extension with you in writing, of at most 30 days, and keep any containment in place until
the fix is live.

We publish an advisory in this repository when the fix is live, or at the extended deadline,
whichever comes first. It describes the vulnerability, its impact and the fix, and it credits you by
name unless you would rather it did not. A Critical or High finding also receives a CVE identifier
once its advisory is published. You are free to publish your own account from the day the fix ships.
If we conclude that a report does not describe a vulnerability, you may disclose it once we have
told you so.

## Safe harbour

We will not take or support legal action against anyone who researches and reports a vulnerability
in good faith under this policy. If a third party does, we will make it known that the research was
authorised.

Good faith means:

- demonstrating a vulnerability on a trading server, chain fork or enclave that you run yourself,
  rather than on a live deployment;
- never accessing anyone else's data beyond what the engine publishes through its log and public
  API, and never changing or destroying it;
- not degrading the service for other users;
- keeping the details private, as this policy asks.

If you come across a vulnerability while using a live deployment, trade only from your own accounts,
stop as soon as you have seen the problem, and report it. To limit your risk, you may then cancel
the affected account's orders and reduce or close its positions, telling us when you do. Until we
have agreed with you how to return what the problem gained the account, including through those
orders, place no other orders on it, and make no withdrawal that would take its balance below that
amount.

If you are unsure whether something you plan to do is covered, ask us before you do it.

## Scope

The policy covers the code in this repository and everything built from it:

| Component | What it is |
| --- | --- |
| `engine-core` | The engine, the deterministic state machine that applies the trading rules. |
| `engine-node` | The trading server, which processes instructions into the engine, publishes the log with its signed checkpoints, and serves the REST API and the WebSocket stream. Commands, configuration keys and output call a running trading server a node. |
| `engine-wire`, `engine-verify` and the browser verifier | The checks of signed answers and attestations, as libraries, `engine` commands and an npm package. |
| The vault and the MONKEY token | The vault holds the USDC collateral on Arbitrum One. It takes deposits, pays signed withdrawals, pays USDC distributions to stakers, and runs the exit. The minter deploys the token, is its only minter, and mints MONKEY against the vault's checkpoints (`Vault.sol`, `MonkeyMinter.sol`, `MonkeyToken.sol`, `ExitVerifier.sol`, `StateTree.sol`). |
| The staking contract | Holds staked MONKEY and its delegations (`Staking.sol`). |
| The operator contracts | Elect the operator, draw its daily turns, take its log heads, promote standbys, prove faults, and take parameter votes (`OperatorRegistry.sol`, `OperatorSchedule.sol`, `LogHeads.sol`, `FaultProof.sol`, `ParameterVote.sol`, `RegistryLens.sol`). |
| The seed relay | Carries each daily turn's random seed from Ethereum to Arbitrum (`SeedRelay.sol`, `SeedReceiver.sol`, `SeedSchedule.sol`, and the relayer in the trading server). |
| The enclave image | The reproducible SEV-SNP image that the trading server runs in, and the attestation that vouches for it (`image/`). |
| `engine ct-watch` | The Certificate Transparency watcher, which runs outside the enclave. |

The [security model](docs/security-model.md) states what each of these components guarantees, and
its known limits.

monkeytrading has not launched, and none of these components holds real funds. The contracts and the
image are in scope as they are published here. No contract has been deployed, and the image has not
yet run on SEV-SNP hardware. Crypto prices come live from Coinbase and Kraken. The vendors for the
other markets, and the worker that signs benchmark interest rates from their publishers, have been
tested only against local stand-ins.

### Findings that matter most

The findings that matter most let someone:

- take funds they are not owed;
- execute an order at a price the rules do not allow;
- make the state the trading server reports differ from the state its published log replays to;
- forge or reuse a signature that the enclave makes;
- read or alter what the enclave holds, from the machine it runs on;
- halt the engine with a request that any client can send.

### Findings that start from an operator's key

The operator runs the trading server and signs its administrative instructions with the operator's
key. The engine is designed not to trust the operator, or the machine the trading server runs on. It
bounds what it takes from that machine's clock, disk and network with a check, except where a known
limit in the security model says otherwise. Whoever runs the trading server cannot:

- change or add an entry in the published log without detection. Cutting the log back is bounded by
  what an on-chain anchor covers, as [the published log](docs/security-model.md#the-published-log)
  describes;
- have the enclave sign a state that its rules do not produce;
- act for an account without its owner's signature;
- credit an account with funds that no deposit brought in;
- pay out funds that the engine did not approve.

A finding that needs an operator's key or credentials to be compromised first is out of scope,
unless it breaks one of these guarantees or gets past one of the engine's checks. The operator's key
can move trading settings only within the bounds that
[trading rules and the operator's bounds](docs/security-model.md#trading-rules-and-the-operators-bounds)
lists, so a change beyond those bounds is in scope too. These guarantees concern whoever runs the
trading server. The vault trusts its timelock's choice of signer, as
[the vault's governance](docs/security-model.md#the-vaults-governance) describes.

### Out of scope

- Volumetric denial of service, meaning attacks that work only by flooding the API or the network
  with traffic.
- Holding every place the API has. The production configuration lets one client address hold 32
  connections, so 16 addresses can hold all 512 connections, and 8 can hold all 256 stream
  subscriber places. A few addresses can therefore deny the whole API for as long as they keep
  their places. An IPv6 client is counted by its /64. The trading server raises
  `api_connections_full` or `api_subscribers_full` while its places are full.
- Social engineering of anyone involved with the project.
- Reorganisations of Arbitrum One that undo a block finalised on Ethereum, which is what the chain's
  `finalized` tag reports.
- Vulnerabilities in third-party services, such as the venues and benchmark publishers that supply
  data, the Arbitrum network, the USDC contract and the enclave hardware. Report those to their
  owners. How the engine handles what these services send it, including a venue that quotes a false
  price, is in scope.

### What counts as a new finding

Each section of the [security model](docs/security-model.md) states what one component guarantees,
then its known limits. A report that rests only on a limit stated there, in an open issue or in a
`TODO` in the code is not a new finding, although its details remain private under the disclosure
timeline above. A way around a check that exists is a new finding.

## Supported versions

monkeytrading has not launched and has no releases. Only the latest commit on `main` is supported,
so please check that an issue still reproduces there before you report it.

## Bug bounty

We reward valid reports of vulnerabilities in the components in scope. The rewards are paid only
from the USDC that the engine distributes once people trade on it. The project holds no fund for
them. An award is approved by a stake-weighted vote, queued in the engine's state, and paid by each
epoch close ahead of the operator's and stakers' shares, up to a cap. It is paid only once the
engine has earned it. Every award is the amount the scale below states for the finding's severity,
so the only judgement in an award is the severity, which we grade by the rules below.

Reports go through the channels in [Reporting a vulnerability](#reporting-a-vulnerability), and
the response times, containment and disclosure rules above apply to them unchanged.

### What the bounty covers

The bounty covers each component in the [scope](#scope) table. A finding is rewarded for the
component whose code or deployment it breaks:

| Component | A finding in scope |
| --- | --- |
| `engine-core` | A way to break a trading rule, the state a command produces, or a guarantee in the [security model](docs/security-model.md#trading-rules-and-the-operators-bounds). |
| `engine-node` | A way to make the trading server publish, sign or serve something its log does not support, to get past one of its checks, or to halt it with a client's request. |
| `engine-wire`, `engine-verify` and the browser verifier | A signed answer or an attestation that a verifier accepts and should refuse, or the reverse. |
| The vault and the MONKEY token | A way to take, freeze or mint funds that the contracts do not allow. |
| The staking contract | A way to take or freeze staked MONKEY or a distribution, or to move votes that the contract does not allow. |
| The operator contracts | A way to elect, draw, promote, fault or vote against the contracts' rules. |
| The seed relay | A way to forge, bias or withhold a daily turn's seed. |
| The enclave image | A flaw in what the image's code and build give the enclave, such as a key, clock or disk guarantee that `docs/enclave.md` states. |
| `engine ct-watch` | A way to make the watcher miss or invent an unattested certificate. |

The contracts and the image are in scope as published in this repository, before they are deployed.
We grade a contract finding by the funds the contract is designed to hold at launch.

### What it does not cover

The [out of scope](#out-of-scope) list above applies, and so does
[what counts as a new finding](#what-counts-as-a-new-finding). The bounty also does not cover:

- a finding that rests on a known limit in the security model, an open issue or a `TODO` in the code;
- a finding that needs an operator's key, or another credential, to be compromised first, except as
  [findings that start from an operator's key](#findings-that-start-from-an-operators-key) describes;
- the development-only parts: the mock TEE, custody and feed providers, the `short-turns` feature,
  the dev configurations, and the keys that `config/test-keys.toml` lists as published;
- `research/`, `agents/`, `docs/`, tests and scripts, unless a flaw there reaches a component in
  scope;
- the output of an automated scanner with no demonstrated effect, a missing hardening measure with
  no path to an effect, and a vulnerability in a dependency that the engine's use of it does not
  reach.

### Who can be paid

You can be paid for a report if:

- it is the first report of the issue that we can reproduce. A later report of the same issue is
  credited, and not paid;
- you followed the [safe harbour](#safe-harbour) when you found it and sent it, and kept it private
  as this policy asks;
- you are not a maintainer, an employee or contractor of the project, or anyone paid to write,
  review or audit the code in question;
- you give an address on Arbitrum One from which you will claim the award;
- you make no demand for payment, or for more than the scale states, as a condition of reporting
  or of keeping a report private.

Several reports with one root cause are one finding, paid once at the highest severity among them.
A fix you send is welcome, and does not change the payout.

### Severity and awards

We grade severity by what an attacker achieves, and not by how hard the finding was to make, using
the same grades as the project's [code review severity guide](agents/skills/agent-code-review/references/severity.md).
The grade is the one whose description fits the worst effect a realistic attacker can reach:

| Severity | What an attacker achieves | Award |
| --- | --- | --- |
| Critical | Funds are lost, frozen or taken: USDC in the vault, MONKEY minted that is not owed, or a claim paid that is not owed. Or a state is forged: the trading server reports a state that its log does not replay to, or the enclave signs a state that the rules do not produce, or a signature is forged or reused. Or a key is controlled: the enclave's keys are read or altered from its host, or the vault's signer, timelock or guardian, or an operator's election, is taken over. | 250,000 USDC |
| High | Wrong behaviour on a realistic path that takes no funds: an order executes at a price the rules do not allow, a request that any client can send halts the engine, an effect is lost or duplicated after a restart, funds are frozen for a time without our action, or one of the engine's checks or an operator's bounds is passed. | 50,000 USDC |
| Medium | Wrong behaviour on an edge path, a failure that nobody would notice, or a refusal of service to one account. | 10,000 USDC |
| Low | A weakness with no effect on behaviour, such as a hardening gap that needs another flaw to matter. | 1,000 USDC |

When a finding fits two rows, it takes the higher one. We grade a finding down one row when it needs
a condition that an attacker cannot bring about, such as the holder of a key choosing to act against
its own interest, and we say so in our reply.

The [findings that matter most](#findings-that-matter-most) grade as follows. Taking funds, a
reported state that differs from the replayed log, a forged or reused enclave signature, and
reading or altering what the enclave holds are Critical. An order at a price the rules do not allow,
and a halt by a request that any client can send, are High, unless they also take funds, which makes
them Critical.

### How an award is decided and paid

1. We grade the finding within the ten business days that [What happens next](#what-happens-next)
   gives, and tell you the grade and the reasons for it.
2. If you disagree, send evidence of a worse effect within ten business days, and we regrade it once.
   Our regrade is final.
3. After the fix has shipped and the advisory is published, a member of the rotation set proposes
   the award on the vote contract. The proposal names your address, the final grade, the scale's
   amount for that grade and the advisory's identifier. Any member may propose an award, and we
   propose one for every finding graded under this policy once its advisory is published. No
   proposal is made while the finding is private, so a vote never reveals a live finding.
4. The rotation set votes, weighted by the stake delegated to its members, and the proposal passes
   at two thirds of the weight, as every parameter vote does. It applies seven days after it is
   logged. The operator cannot register an award by its own command, and the fast path refuses one.
5. The engine adds the award to its queue with the amount the scale states for the grade, and
   refuses any other amount. It awards each advisory once, and refuses a second award that names an
   advisory already awarded. The queue and each award are public in the log and in the API.
6. Each epoch close pays the queue first, oldest award first, before it splits the rest of the
   excess between the operator and the stakers. A close pays awards at most 5% of its
   distributable excess, and never more than 50,000 USDC. An award that a close cannot finish stays
   at the front of the queue with the rest of its amount.
7. A payment is credited to your address as a distribution entitlement, and you claim it from the
   vault with a proof, as a staker claims a distribution. Nothing is paid until you claim.
8. We credit you in the advisory as [Coordinated disclosure](#coordinated-disclosure) describes.

A report is paid only from distributions that the engine has earned, and an epoch distributes
nothing while the house's net asset value is at or below the liquidity floor or nobody is staked.
An award waits in the queue until a close can pay it. The queue's state and each payment can be
checked by replaying the log, and each claim against a checkpoint root.

The vote is only as independent as its weights. At launch the rotation set's stake is largely the
project owner's, so the owner's votes decide which reports are awarded. What the engine enforces,
whoever votes, is the scale's amount per grade, one award for each advisory, the cap on each close,
the queue's order and that a paid award is final.

If the vault's permanent exit starts, the part of an award already credited to your address ranks
with the stakers' distributions in the exit, and you claim it as they do. The part still in the
queue when the exit starts is lost.

The scale is fixed in the engine's rules. A vote may move the cap to between 5% and 20% of a
close's distributable excess, and its limit to between 50,000 and 250,000 USDC. The operator cannot
change the scale or the cap, or register an award, by its own command.
