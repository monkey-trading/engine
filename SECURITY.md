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
| The vault and the MONKEY token | The vault holds the USDC collateral on Arbitrum One. It takes deposits, pays signed withdrawals, mints MONKEY, pays USDC distributions to stakers, and runs the exit. It deploys the token and is its only minter (`Vault.sol`, `MonkeyToken.sol`, `ExitVerifier.sol`, `StateTree.sol`). |
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

There is no bug bounty yet. We will announce one, with its scope and rewards, before launch.
