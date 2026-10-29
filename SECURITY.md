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
the custody vault contract that will hold collateral on Arbitrum One, with the MONKEY token it
deploys and alone mints against posted checkpoints, the staking contract that holds MONKEY, and
the vault’s USDC distribution claims; and the enclave image the node runs in, together
with the attestation that vouches for it. The vault contract takes deposits, pays signed withdrawals
with approval inclusion proofs, and mints MONKEY entitlements, and is in scope as published here;
the enclave image, built from `image`, is in scope as published here too. A trading account must be
able to sign requests with its own key, since the engine accepts no contract signatures: a contract
wallet funds a key that signs for it through the vault's `depositFor`, and a balance credited to a
contract account can be recovered only through the exit (#48), so a report of that alone is not a
new finding.

The findings that matter most let someone take funds they are not owed, fill an order at a price the
rules do not allow, make the state the node reports differ from the state its published log replays
to, forge or reuse a signature the enclave makes, read or alter what the enclave holds from the
machine it runs on, or halt the engine with a request any client can send.

The node signs every API response and stream frame a client relies on with the attested enclave
key, over the request or connection it answers and a challenge the client chose, so a client that
sends a fresh challenge can tell a forged, replayed, or substituted answer from the node's own, and
a stream frame dropped, repeated, or taken from another connection from one that fits. The
signatures do not stop anyone between the client and the node from delaying or dropping traffic:
the stream's signed heartbeats let a client detect that, not prevent it. Five limits are by design,
and a report that rests only on one of them is not a new finding. A signature binds an answer to the
bytes of the request and challenge it answers, not to one delivery of them: someone in between can
send the node a copy of a request, or open a copy of a stream connection, and the node answers each
copy, so a refusal can be the node's answer to a second copy of a request whose first copy ran. A
client therefore confirms a refusal of an order, cancel, withdrawal, or other state-changing
request with a signed read of the account under a fresh challenge before showing it; the account's
next nonce alone does not show that the request ran, since a request refused for its expiry uses
its nonce too. A response to a request sent without a challenge can be replayed to another such
request. A browser's preflight, a response the origin check, the rate limiter, or the body read
refuses, and every answer while trusted time does not bound the host's clock are unsigned (a signed
stream connection closes instead). The attestation, the archive, and health are unsigned, because
they prove themselves or carry nothing to rely on. The time an answer carries is the host's clock,
signed only while it is within the configured drift bound (`time.max_drift`) of trusted time, so it
is accurate to that bound.

The node's development settings, which are the mock enclave, mock custody, simulated prices, and
historical unsigned requests, exist for local testing and make no security claims of their own.
Neither does `POST /v1/dev/deposit`, which the node serves only with mock custody, nor the browser
verifier's `verifyMockAttestation`, which reads a mock attestation and reports it as a mock. A weakness in
them is in scope when a production configuration can reach it, or when whoever runs an attested enclave can
switch them on, since the node reads its configuration from the machine it runs on. A production
configuration uses hardware attestation, on-chain custody, live prices, and signed requests.
Requests are signed, and the node refuses unsigned ones unless both the enclave and custody are
mocks, and a production build of the node refuses every mock provider.
Under V24 and V26 onward, client actions and eligibility declarations require signatures even with mock providers,
and declarations, orders and withdrawals require fresh screening of the account and client peer.
A missing declaration, an unknown classification, or a screening source failure keeps a deposit
uncredited and pending for bounded automatic retries.
Confirmed sanctioned holds have no automatic release or refund path.
Older logs retain their recorded authorization rules, including the standalone pre-access V25 rules. Access-source evidence is signed by a recorded
authority and bounded by freshness, subject and request digest; the source classifications and
host-supplied initial configuration still require approved provenance and attestation binding.
API jurisdiction screening uses the socket peer address, or, behind a configured pass-through proxy,
the client address in that proxy's PROXY protocol header. The host's network supplies both, so it
is not yet bounded against a host that relays or translates client connections.
The local source fixture is synthetic and its published signer is refused with real attestation.
Sanctioned funding remains uncredited and held with public evidence. The other three are not
finished: the node builds into a reproducible SEV-SNP image and requests an attestation report
through configfs-tsm, and `engine verify-attestation`, and the browser build of the same code,
verify a quote's certificate chain, measurement, policy, and configuration against a release, but
booting that image on SEV-SNP hardware and the verifier against a hardware quote are still being
validated, as are live prices
and the separate signed benchmark publisher worker. The node relays the vendor's certificate chain
and revocation list for verifiers that cannot call the vendor; a verifier pins the vendor roots
itself, but a host that blocks the node's fetches delays a revocation until the served list's
`nextUpdate`. On-chain custody credits vault deposits and
pays signed withdrawals, exercised on a local chain; production deployment remains out of scope
for the development scenarios.

Out of scope are volumetric denial of service, meaning attacks that work only by flooding the API or
the network with traffic; social engineering of anyone involved with the project; reorganisations of
Arbitrum One that undo an Ethereum-finalised block (the L2 `finalized` tag); and vulnerabilities in
third-party services, such as the venues and benchmark publishers that supply data, the
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
evidence does. Withdrawal completion evidence also requires that independently trusted signer
and its deployment before replay can clear an approval. Signatures expire after 24 hours and bind
the current signer epoch; an expired one is replaced only on its account's signed request, which access
screening checks like a new withdrawal, never by a signer rotation, on a schedule or on restart; a
rotation replaces only an authorization still usable, and the vault rejects retired epochs immediately. Accepted checkpoint history
authenticates historical roots; immutable signer-epoch history independently authenticates reissues
against older roots during recovery and custody-backed replay. The unanchored checkpoint tail still
requires the current signer. The deployment's multisig proposes changes through its nonzero-delay
timelock, which alone rotates the signer or changes deposit/withdrawal pauses. Latest transaction
inclusion alone never authorizes withdrawal release; checkpoint startup and release use custody's
withdrawal finality tag, independently of the latest credit tag and its confirmation depth. New
withdrawals pause while the latest finalized checkpoint's custody prefix is unproven against canonical
receipts, and resume once a checkpoint with proven coverage reconciles; a failed chain read never
lifts a pause or erases a proved divergence.
Hardware EVM custody requires this protection. Staking mirror references and completed scan anchors enter the state root from V27; writable
recovery of older staking commands also requires authenticated origin seals. Optional recovery snapshots require independently
provisioned attested signer pins and authenticate the complete snapshot, its deployment, and its
checkpoint; with them, account history and the segment catalog are read and updated only through
proofs against roots the node signed. Their signatures establish integrity, not freshness; the same
independent custody preflight still applies. Full `engine replay` checks skipped historical log
segments.
Some checks are still being built: the alert outbox persists pages on host disk, whose schema and
rows it checks but whose deletion or rollback it cannot detect, and its cross-restart age budget
uses an unsealed host clock; without snapshots, the
account history index is read from host disk with only its schema checked, and is matched to the log
by one command, the newest it has taken, though SQLite reads it in defensive mode and each event it
serves names its place in the log, where anyone can check it; the sequencer and the signed answers,
which bound the host's clock with signed time, trust the guest's monotonic clock without yet
checking that the host cannot scale it; hardware enclave signing keys do not yet survive a restart (#28), so a hardware node cannot
restart after a crash or a drained shutdown: its vault must name the enclave's key, and the old key's
commands and checkpoints after the last one the vault accepted cannot be authenticated under a new
key; the dev configurations explicitly
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
host-supplied configuration, which only a verifier holding a published copy of it can check against
the attested config hash. From V20, deposits trade on sequencer confirmation (`latest`, less
configured confirmations), while principal can be withdrawn only after Ethereum finality (`finalized`).
Hardware-TEE validation refuses a different tag pair. Exact receipts, unfinalised principal and
finalised scan progress are deterministic sequenced state. Legacy EVM credits gain no finality from
their old tag, credit status, host cursor or a config change: activation records their verified credit
ledger and locks unresolved principal without another cash credit. Exact canonical receipt proof
and withdrawal finality release each lock independently. Missing or ambiguous historical identity
keeps that principal locked with Critical; an adopted receipt uses the normal reversal path. Recovery
refuses to sign a stranded legacy approval that consumes unresolved principal until independent
finality proof succeeds, before opening the public log for writing. Inherited unsigned approvals retain
their authenticated approval prefix across activation; later funding requires checkpoint coverage of
its own sequence before signature release. Finality proof authorizes only the specific inherited
obligations it proves safe and never applies cash again. A quorum-proven pre-finality reversal
debits once, raises Critical and runs the existing executable close-out/flat-account house write-off;
stale positions wait for prices, and negative house NAV does not halt trading. A deposit reincluded in another block with the same beneficiary and amount moves to its
replacement receipt in one transition, including at a changed log index: cash nets out, no close-out
runs and Critical is still raised. An orphan with no replacement up to the eligible head is debited.
Pre-finality reversals and handling of missing quorum
are in scope. Earlier protocols keep a host cursor whose advancement can skip deposits; checkpoint reconciliation detects the resulting deposit-count mismatch and escalates after its configured catch-up deadline. The checkpoint transport trusts its separately configured RPC endpoint; deposit quorum does not automatically protect that client.
The enclave image's kernel takes input from its host that the launch measurement does not cover,
without yet bounding it: the data disk's filesystem, the ACPI tables, the emulated devices, and the
network configuration.
With `[api.tls.acme]`, the node has a public CA certify its attested TLS key for the API's domain,
so a browser can trust the API without checking the attestation; a browser that does not check it
also trusts any other certificate a CA issues for the domain. The host can obtain one for a key
outside the enclave by answering the CA's TLS-ALPN-01 validation on the port it forwards, as can
whoever controls the domain's DNS. The domain's CAA record narrows issuance to one CA and
TLS-ALPN-01 but not to the enclave, since the enclave's ACME account changes at every start until
its keys survive a restart (#28). `engine ct-watch` raises an alert for such a certificate once a
followed Certificate Transparency log shows it, and must be running before the first one is issued.
Databento's live gateway, one of the sources for futures and US share prices, offers no TLS, so the
host can alter its quotes in flight; the divergence bound limits how far that moves a market priced
from it and one TLS source, and beyond that the market has no price. Before a futures roll the host
can move the two contracts' prices apart, so the roll's gap shifts by up to the full divergence
bound and the market's index keeps that shift after the roll. The
Certificate Transparency watcher (`engine ct-watch`) checks tree head signatures but not consistency
or inclusion proofs, so a log that shows different views to different clients can hide a certificate
from it, and it sees only the logs it follows. A
gap that this policy, an open issue, or a `TODO` in the code already records is known, so a report
of it is not a new finding, though its details stay private under the disclosure timeline above; a
way around a check that exists is a new finding.

## Supported versions

monkeytrading has not launched and has no releases. Until launch, only the latest commit on `main`
is supported, so please check that an issue still reproduces there before reporting it.

## Bug bounty

A bug bounty, with its scope and rewards, will be announced before launch.
