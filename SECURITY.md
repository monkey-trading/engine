# Security policy

monkeytrading is meant to be trusted without trusting the people who run it: its rules are public
code, it is built to run inside a confidential-computing enclave, and anyone can replay the commands
it publishes to check the state it reports. A vulnerability can undermine any of that, so we ask
that you report it privately and give us time to fix it before it becomes public. This policy
explains how to reach us, what you can expect from us in return, and what it covers.

## Reporting a vulnerability

Email reports to william.jevons@proton.me. Please do not describe a vulnerability in a public
issue, pull request, or discussion, even in outline. Mail sent from a Proton account is end-to-end
encrypted; a PGP key for senders on other providers will be published in this file. Either way,
please keep the details out of the subject line.

A report is easiest to act on when it names the affected component and the commit of `main` you
tested, says what an attacker can achieve and what access they need first, and shows how to
reproduce it. A failing test, a command log that replays to a different state from the one the node
reported, or a script against a local node makes the strongest case. Tell us, too, whether you have
a fix in mind, whether anyone else knows of the issue, and how you would like to be credited.

## What happens next

We aim to acknowledge a report within three business days, and within ten business days of
receiving it to tell you whether we can reproduce the issue, how severe we judge it, and what we
plan to do. We aim to fix a vulnerability within 90 days of your report, or within 30 days if it is
of a kind that the Scope section below lists as mattering most, and we will keep you informed until
it is resolved. If we conclude that a report does not describe a vulnerability, we will explain
why.

## Coordinated disclosure

We ask that you keep the details of a vulnerability private until a fix has shipped, or until 90
days after you reported it, whichever comes first. A fix has shipped when it is on `main` and, once
the engine is live, running in production. If a fix needs longer, we will tell you why before the
deadline and agree a new date with you. Once the fix has shipped, we publish an advisory in this
repository that describes the vulnerability, its impact, and the fix, and credits you by name
unless you would rather it did not. You are free to publish your own account from the day the fix
ships. If we conclude that a report does not describe a vulnerability, you may disclose it once we
have told you so.

## Safe harbour

We will not take or support legal action against anyone who researches and reports a vulnerability
in good faith under this policy, and if a third party does, we will make it known that the research
was authorised. Good faith means demonstrating a vulnerability on a node, chain fork, or enclave you
run yourself rather than on a live deployment; never accessing anyone else's data beyond what the
engine publishes through its log and public API, and never changing or destroying it; not degrading
the service for other users; and keeping the details private as this policy asks. If you come
across a vulnerability while using a live deployment, trade only from your own accounts, stop as
soon as you have seen the problem, and report it. To limit your risk you may then cancel the
affected account's orders and reduce or close its positions, telling us when you do. Until we have
agreed with you how to return what the problem gained the account, including through those orders,
place no other orders on it and make no withdrawal that would take its balance below that amount.
If you are unsure whether something you plan to do is covered, ask us before you do it.

## Scope

The policy covers the code in this repository and what is built from it: `engine-core`, the
deterministic state machine that applies the trading rules; `engine-node`, which sequences commands
into it, publishes the log and its signed checkpoints, and serves the public REST and WebSocket API;
the custody vault contract that will hold collateral on Arbitrum One; and the enclave image the node
runs in, together with the attestation that vouches for it. The vault contract accepts deposits and
pays signed withdrawals with approval inclusion proofs, and is in scope as published here; the enclave image is still being
built, and is in scope as soon as its code is published. A trading account must be able to sign
requests with its own key, since the engine accepts no contract signatures: a contract wallet funds
a key that signs for it through the vault's `depositFor`, and a balance credited to a contract
account can be recovered only through the exit (#48), so a report of that alone is not a new
finding.

The findings that matter most let someone take funds they are not owed, fill an order at a price the
rules do not allow, make the state the node reports differ from the state its published log replays
to, forge or reuse a signature the enclave makes, read or alter what the enclave holds from the
machine it runs on, or halt the engine with a request any client can send.

The node's development settings, which are the mock enclave, mock custody, simulated prices, and
unsigned requests, exist for local testing and make no security claims of their own, and neither
does `POST /v1/dev/deposit`, which the node serves only with mock custody. A weakness in them is in
scope when a production configuration can reach it, or when whoever runs an attested enclave can
switch them on, since the node reads its configuration from the machine it runs on. A production
configuration uses hardware attestation, on-chain custody, live prices, and signed requests.
Requests are signed, and the node refuses unsigned ones unless both the enclave and custody are
mocks. The other three are not finished: the node can request an attestation report through
configfs-tsm, but attestation on SEV-SNP hardware and a verifier for it are still being built, as
are live prices and the separate signed benchmark publisher worker. On-chain custody and withdrawal
claims are exercised on a local chain; production deployment remains out of scope for the development scenarios.

Out of scope are volumetric denial of service, meaning attacks that work only by flooding the API or
the network with traffic; social engineering of anyone involved with the project; reorganisations of
Arbitrum One that undo a block after its batch reached Ethereum, the finality the custody adapter
waits for; and vulnerabilities in third-party services, such as the venues and benchmark publishers that supply data, the
Arbitrum network, the USDC contract, and the enclave hardware, which should be reported to their
owners. How the engine handles what those services send it, including a venue that quotes a false
price, is in scope.

Findings that need an operator's key or credentials to be compromised first are also out of scope,
unless they defeat a guarantee the engine makes. The engine is designed not to trust whoever runs it
or the machine it runs on: it bounds anything it takes from that machine's clock, disk, or network
with a check, and whoever runs it cannot alter the published log without detection, have the enclave
sign a state its rules do not produce, act for an account without its owner's signature, credit an
account with funds that no deposit brought in, or pay out funds the engine did not approve. A
finding that breaks one of these guarantees, or gets past one of those checks, is in scope even when
it starts from an operator's key. With `[checkpoints]` configured, startup verifies the replayed log
against the latest posted vault commitment at custody finality and refuses a shorter or conflicting log before serving or opening
history for writes; checkpoint signatures verify
against their chain, vault, and named signer. New posts also sign the exact predecessor sequence
and root, which the vault compares atomically to fence divergent same-key publishers. The checkpoint RPC still supplies the canonical chain
view, and a signature's named signer must be checked against a trusted attestation or the vault's
authorized signer. A withdrawal signature never settles its approval: finalized custody nonce-use
evidence does. Signatures expire after 24 hours and bind the current signer epoch; the vault rejects
retired epochs immediately. Accepted checkpoint history authenticates historical roots; immutable
signer-epoch history independently authenticates reissues against older roots during recovery and
custody-backed replay. The unanchored checkpoint tail still requires the current signer.
The deployment's multisig proposes changes through its nonzero-delay timelock, which alone rotates
the signer or changes deposit/withdrawal pauses. Latest transaction inclusion alone never
authorizes withdrawal release; the configured custody finality tag and confirmation depth apply.
Hardware EVM custody requires this protection. Some checks are still being built:
the alert outbox persists pages on host disk, whose schema and rows it checks but whose deletion or
rollback it cannot detect, and its cross-restart age budget uses an unsealed host clock; the
account history index is read from host disk with only its schema checked, and is matched to the log
by one command, the newest it has taken, though SQLite reads it in defensive mode and each event it
serves names its place in the log, where anyone can check it; the sequencer, which bounds its host's
clock with signed time, trusts the guest's monotonic clock without yet checking that the host cannot
scale it; hardware enclave signing keys do not yet survive a restart; the dev configurations explicitly
use a published mock-only signer, which real attestation refuses; the custody adapter requires a strict
majority of at least three configured independent RPC providers to agree on deposits, block hashes
and finality, with reads compared at the same block height and empty log responses requiring the
answering provider to serve the range's final block. A dissenting or unavailable provider alerts;
lost quorum raises a Critical alert and pauses crediting while trading continues. Providers excluded
by that poll's identity vote cannot supply later votes, and exclusion never lowers the configured
majority threshold. This also rejects a detected wrong-chain provider paired with a malicious
provider mirroring its transcript. The single-fault guarantee still assumes an honest majority
serves the intended chain and token; endpoint independence alone cannot establish that. It still trusts
a compromised majority, operator independence is not machine-verifiable, and it takes the vault,
its token, provider endpoints and finality tag from the
host-supplied configuration, and keeps its deposit cursor on host storage, where a cursor moved
forward skips deposits; checkpoint reconciliation detects the resulting deposit-count mismatch
and escalates after its configured catch-up deadline; and the node refuses unsigned requests unless both the
enclave and custody are mocks, but does not yet refuse mock custody or simulated prices when it runs
with hardware attestation, whose report vouches for the signing key but not for the configuration. A
gap that this policy, an open issue, or a `TODO` in the code already records is known, so a report
of it is not a new finding, though its details stay private under the disclosure timeline above; a
way around a check that exists is a new finding.

## Supported versions

monkeytrading has not launched and has no releases. Until launch, only the latest commit on `main`
is supported, so please check that an issue still reproduces there before reporting it.

## Bug bounty

A bug bounty, with its scope and rewards, will be announced before launch.
