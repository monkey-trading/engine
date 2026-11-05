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
with approval inclusion proofs, mints MONKEY entitlements, and, once checkpoints stop, runs the exit
against the last posted root, and is in scope as published here;
the enclave image, built from `image`, is in scope as published here too. A trading account must be
able to sign requests with its own key, since the engine accepts no contract signatures: a contract
wallet funds a key that signs for it through the vault's `depositFor`, and a balance credited to a
contract account can be recovered only through the exit (#48), so a report of that alone is not a
new finding. The staking contract keeps each staker's delegations as amounts, with no limit on how
many candidates one staker delegates to: an unstake larger than the staker's undelegated stake
reduces every one of its delegations in one transaction, so a staker with very many delegations
takes some back first, each call touching one or two, before such an unstake fits in a block. That
cost falls only on the staker that chose the delegations, and no one else can add to them, so a
report that rests on it alone is not a new finding.

The seed relay carries the RANDAO mix that seeds each daily operator turn: `SeedRelay` on Ethereum,
`SeedReceiver` on Arbitrum, and the relayer every rotation-set member's node runs under
`[seed_relay]`. The relay sends a seed only with a proof against a beacon block root the EIP-4788
contract holds: the header hashes to that root, its state's `fork.current_version` is one the
deployment accepts, its slot lies past the epoch's last slot and before `randao_mixes` reuses the
epoch's index, and the state holds the seed at that index. The receiver takes a seed only from the
relay's aliased address, only for the epoch the schedule fixes for its turn, and only before that
turn's draw deadline, from which its answer for the turn is final. What it still trusts:
- Ethereum's consensus, for the roots; the mix itself is biasable by the proposers of the epoch's
  last slots, each of whom can withhold a block and so choose between two mixes.
- Arbitrum's inbox, for delivery. Arbitrum One's sequencer can hold a relay's message for 24 hours
  before anyone may force-include it, and a relay that misses the draw deadline is refused, so the
  turn draws from the last relayed seed hashed with its number. The schedule leaves a relay two days
  for that reason; whoever can withhold every relay until a deadline chooses between the mix and
  that fallback. No turn draws before the first seed arrives.
- Its relayers, for liveness: nothing relays by itself, and a turn that reaches its deadline without a
  seed raises `seed_missing`.
- The beacon state's layout. The relay's indices are those of the `BeaconState` from Electra on, whose
  fields fill a 64-leaf tree, and it accepts only the fork versions it was deployed with; the relayer hashes only
  Fulu states. Any later fork stops relays until a node release reads its state and, unless its
  version was listed at deployment, a new relay and receiver are deployed; turns draw from the
  fallback meanwhile. Epochs are counted from 12-second slots.
The relayer's beacon node, Ethereum providers and signing endpoint are host-reached and untrusted:
each read it acts on is a strict majority's or is checked again on chain, so a wrong answer costs a
relay, never a wrong seed.

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
A wallet can declare and manage its session keys before its first deposit, so the fresh-account rate
budgets, rather than a deposit's cost, bound what a free wallet can add to the public log and to state,
and a client that exhausts the shared budget refuses other new wallets until it refills. A wallet with a pending deposit declares past those budgets, against a budget of its own.
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
with a check, and whoever runs it cannot change or add an entry in the published log without
detection (cutting the log back is bounded by what an on-chain anchor covers, as below), have the enclave
sign a state its rules do not produce, act for an account without its owner's signature, credit an
account with funds that no deposit brought in, or pay out funds the engine did not approve. A
finding that breaks one of these guarantees, or gets past one of those checks, is in scope even when
it starts from an operator's key. With `[checkpoints]` configured, startup verifies the replayed log
against the latest posted vault commitment at custody finality and refuses a shorter or conflicting log before serving or opening
history for writes; checkpoint signatures verify
against their chain, vault, and named signer. Those checks reach only as far as the vault's
checkpoints, so whoever runs the node can still choose after the fact whether its own recent
commands happened, in two ways (#541). It can hold a batch's data sync while it watches the market,
then let the sync finish or kill the node: replies, stream entries, and signed withdrawals go out
only once their batch is durable, so until then nobody outside has seen it, and a batch the
restarted log lacks never happened. Once the oldest unsynced batch has waited 5 s for its sync,
the sequencer takes no further request and raises Critical `log_sync_held`, so nothing is stamped
behind a held batch and a hold of the disk alone is raised within 5 s. The webhook page commits to the
alert outbox on the same disk first, so a hold of the whole disk delays it and a kill during the hold
loses it; the `engine_log_sync_held` gauge, served from memory on the metrics listener, reports the
hold meanwhile (`EngineLogSyncHeld`), though a host that holds the disk can also answer for the
listener. The snapshot capture's sync, the checkpoint's catalog maintenance, a rotation's catalog
fold, and a handover's segment seal write on the sequencer's thread outside this bound: a hold there
stops sequencing too and pages as `sequencer_stalled`. What the hold can gain the host is bounded
the same way as its ordinary trading: its commands still price at their stamps under the size,
stress, and toxicity rules, and the house's exposure caps still bind what it can open. The hold
lasts only as long as the host stalls every reply and every new stream entry, which every client
sees, and the signed heartbeats keep naming the old head unless the host holds the network too.
Nothing anchors a batch before its release, so posting log heads on chain does not close this: a
host that kills the node before a post can withhold the post too. It can also cut the log back and
restart the node, erasing commands the node had already answered and streamed. With `[log_heads]`
configured the bound on an undetected cut is
the commands after the latest posted head: the enclave signs its newest released `(seq, prefix)` —
the cumulative command-prefix digest, a running hash over every command's sealed bytes that
checkpoints do not reset, so a posted head commits to the whole history — under an
`engine.log-head.v1` signature,
off the sequencer, and a relayer posts it to a companion contract that checks the signer against the
vault's `checkpointSigner` and `signerEpoch` and keeps the highest seq per epoch. Before the
restarted node writes anything, it reads the posted head through the custody RPC quorum and refuses
to start on a log that lacks it, naming the missing range or the seq whose link differs (Critical
`log_rollback`), and a posted head that overtakes the running node — a higher seq, or the same seq
naming another prefix — freezes sequencing. On chain the posts form a predecessor chain: each post
names the previously accepted head it extends, so a longer divergent copy cannot replace the
anchor without containing it. The running node adopts an on-chain head as the parent of its next
post only when the head's prefix is the one its own log holds at that seq — the key proves
nothing here, since a second copy of it signs correctly too: a head naming another prefix at a seq
the node retains freezes sequencing, and a head at a seq it retains no prefix for is not adopted,
so posting halts (`log_head_stalled`) until a restart checks the posted head against the whole
log. Posts go at most every
`log_heads.interval` (an hour by default) and only when the head moved, so a cut must land within
about the posting interval plus chain inclusion of the log's end to pass; a host that stalls or
withholds the poster's submissions extends the window by however long the tail stays unposted,
and `log_head_stalled` pages once the head has not advanced for three intervals: counted, until a
read sees it advance, in trusted time since a stamp that recovery authenticates — the command of
the head startup checked, or, while no epoch this key signed holds one, the command the
vault-finalized checkpoint covers — from once the restarted poster's first post has had a posting
interval to be included, so neither cutting its log back nor restarting it less often than once
per posting interval keeps the alarm quiet. A host that restarts it more often, with each stop under
the node-down alert's minute, keeps `log_head_stalled` quiet; blocked submissions still raise
`log_head_post_failed`, and every start logs. While no epoch this key signed holds a head, the
rollback bound is the vault-finalized checkpoint's, as without `[log_heads]`, and a restart counts
from the newest such checkpoint, so restarts less than three intervals apart keep the alarm quiet
while checkpoints finalize.
Without `[log_heads]`, the bound is the commands after the latest checkpoint the vault has
finalized: a cut below a later accepted-but-unfinalized checkpoint shows when the node's checkpoint
poster next compares a checkpoint with the vault (`checkpoint_conflict`), and a cut after the latest
accepted checkpoint goes unnoticed by the node. Checkpoints go out about once per finality
delay of the custody chain, so about two finality delays of commands can be erased while
checkpoint delivery is healthy; a host that stalls or withholds the poster's submissions
extends the window by however long the accumulated tail stays unanchored. Clients
keep the signed replies and frames for them, but the node does not compare those with the
restarted log. A standby following the operator's signed stream does: it takes a command only when a seal a checkpoint the copy
holds names (or the followed enclave's) continues the standby's copy, so history sealed
before a handover still extends it, and each stream it resumes must first repeat
the copy's last command exactly. When the operator's log no longer holds what the standby copied (a
signed heartbeat behind the copy, a repeat that differs, or a command that does not chain), the
standby raises the Critical `operator_log_rollback` alert, through its own alert routing, naming
the range; it takes nothing more and keeps its copy unchanged to restore from. New posts also sign the exact predecessor sequence
and root, which the vault compares atomically to fence divergent same-key publishers. The standby's
check reaches only as far as what it received: it detects a cut that goes back past that. Posted log
heads are per signer epoch, so a standby's takeover — which drops the outgoing key's tail by design —
is not refused: a new epoch starts with no posted head, and its key signed no earlier one. A key
`setSigner` reinstalls, as after the guardian retires it, starts a new empty epoch too, so while the
current epoch holds no head and the starting key signs it, startup walks back through the epochs
before it for as long as that key signed them and checks the newest head one holds, all at one
quorum-agreed block: the reinstalled key's own earlier head still binds its log. The checkpoint RPC still supplies the canonical chain
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
timelock, which alone rotates the signer, lifts a withdrawal pause or changes the deposit pause, or
names the release that may hold the enclave's keys: the one launch measurement and config hash it
sets as current while operator rotation is off, and, while it is on, each release it approves for
the operator in effect, whose candidate account moves the vault's release to it, withdrawing the
approval of the release it leaves. As the vault's immutable guardian, the same multisig may also, without the delay, pause
withdrawals for seven days or retire the signer, which the vault then refuses for withdrawals and
checkpoints until the timelock installs one, and for MONKEY and USDC claims until that signer posts
a checkpoint, which an exit that starts first rules out; the node signs no withdrawal while either
holds. The
node's own credentials, its RPC provider keys, market-data vendor keys, and alert webhook URLs, reach
it only encrypted to a one-time key its quote proves an enclave of the release holds, which the
operator's tool checks against the release manifest first, under the signature of the operator key
in effect: the attested config names which secret each field reads, and fixes each RPC provider's
origin, so a bundle, even one a stolen operator key signs, can change a provider's key but never which
provider credits deposits. They are sealed per release, a recorded delivery does not decrypt on a later
connection, and an election moves none of them. A host that restores an older sealed file gets the
older bundle, whose version the enclave's attested status shows; the operator revokes a rotated key at
its provider, since nothing bounds that restore more tightly. With
`[tee.sealing]` the keys are sealed on the host's disk under a key the SEV-SNP firmware derives from
the chip, the launch measurement, the guest policy, and the committed TCB, with the config hash
bound to the ciphertext, so neither the host nor another release, the same code under another
config included, can open them. A node opens or seals keys only while the vault names its release
as current at custody finality, and only in the vault's current generation of it, which every
change of the current release raises, and keys are sealed or handed over only once that generation
has folded the change, so a release the timelock has superseded never signs again
from its old sealed file, even once it is named again; the keys move to a new release only after the vault names it and
each release attests the other over a challenge fresh to the connection, and only encrypted to the
new release's attested one-time key, which takes them only if the vault already names their
signing key; a release that has stopped to hand them over serves new releases until one confirms. Two copies of the current
release that the host runs from copies of one disk can both open the keys; the vault takes one
copy's checkpoint and the other stops on the conflict. A node the vault does not name as signer
signs nothing and serves only its attestation. Latest transaction
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
serves names its place in the log, where anyone can check it; the SEV-SNP sealing key, the
handover's hardware quotes, and the image's Secure TSC launch are built to the firmware's guest
interface but not yet run on SEV-SNP hardware (#108), and the
mock TEE's stand-in for sealing is a public key; two copies of one release unseal the same keys, and
while the vault refuses the checkpoints of all but one, the other's signed API answers still verify;
a release taking over the keys checks the running release's certificates at the host's clock, which
the vault naming the key bounds; a superseded release refuses its own sealed copy only by reading the
vault's current release through the checkpoint RPC endpoint, which the node trusts for the vault's
state, and a flaw in a release that has run still needs a signer rotation; from V37 the admin key
follows the operator registry's elections, which the engine mirrors from finalised chain events, so
the incumbent's host can delay an elected key's takeover, bounded only by the custody watcher's
stall alerts, and from V50, which splits the operator's share of each distribution among the
operators by their sequenced time in office, the incumbent keeps earning the share through that
delay; from V51 a registry that names a schedule rotates the operator daily instead: each election
close adopts a rotation set seven days plus the finality margin before it takes effect, and anyone
draws each 22:30 UTC turn from it a day ahead with a seed relayed from Ethereum, so whoever can
withhold every relay until a turn's draw deadline chooses between that turn's seed and the fallback;
a turn's key signs from its boundary only when a set mirrored at least seven days earlier names it;
the vault names the turn's release, in the generation the schedule fixed, from the boundary without a
transaction, and both enclaves of a handover accept the turn's member from the finalised schedule
once their trusted time reaches the boundary, so a host that holds an enclave's Roughtime replies
back can delay a handover but not bring it forward; the member earns the operator's share only from
its enclave's `TurnTaken` to the end of its turn, and an incumbent past its turn earns nothing;
promotion over turns, which is the liveness rule's, is not built yet, so a failed member holds its
turn until the next boundary and the governance fallback (rotation off and `installOperator` once the
drawn turns end) applies; the enclave keys and the vault's current release do not yet follow an election (#44); the dev configurations explicitly
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
TLS-ALPN-01, and, with sealed keys, an `accounturi` naming the enclave's ACME account narrows it to
that account; a host that deletes the sealed keys forces new ones, which the vault must register.
`engine ct-watch` raises an alert for such a certificate once a
followed Certificate Transparency log shows it, and must be running before the first one is issued.
Only a source reached over TLS that ends in the enclave sets or moves a price: every venue endpoint
is `wss://` on the venue's own domain, its certificate checked against roots compiled into the
binary, so the host can delay or drop a source's quotes but not alter them. Certificate validity is
judged by the host clock and revocation is not checked, so a host holding a venue key that has
expired or been revoked could pose as that venue. A production build refuses a config in which fewer
venues quote a genesis market, or any futures contract its genesis listing names, than its
`min_sources`; a market listed or relisted after genesis is watched only by the `market_unfed`,
`market_no_price` and `futures_roll_overdue` alerts.
A price's age under a hostile host is bounded on the monotonic clock, not the wall clock. The host
carries every byte and sets the wall clock, so it can hold back a venue's TLS stream and set its clock
back to match, and a quote's venue stamp then looks fresh: the drift bound alone would let prices run
`time.max_drift` plus `max_age_ms` behind the market. So each TLS venue connection pings its venue
every `feeds.venues.ping_interval`. A TLS stream is ordered, so a pong arrives only after every frame
the venue sent before it, and a frame read after that pong was sent after its ping. A quote is
sequenced only within `feeds.venues.max_delay` of the newest ping answered when it was read, whether
the host held it back on the network or held the sequencer back by stalling its log or its CPU.
Sequenced time keeps pace with monotonic time whatever the host clock does, giving up at most 10 ms
of it a second, and after a restart stamps hold, and venue quotes are dropped, until that pace from
the last stamp passes every quote carried across it (for a market whose `max_age_ms` is longer than
`time.max_drift`, only as far as trusted time admits), so a quote counts for about its market's
`max_age_ms` at most once it reaches the engine. A stall after a command is stamped delays that
command but not the stamp of any after it. A price an order fills at is therefore at most about
`max_delay` plus `max_age_ms` old, from when the venue sent it: 3 s under production's 2 s and 1 s,
whatever the host clock reads and however the host delays the network. A venue sends a book only
when it changes, so on a Coinbase, Kraken or Bitstamp connection a pong with no change to a symbol's
book since one read before its ping went out sequences that book's mid again, stamped no later than
the venue's clock, or the host clock, when the ping went out, and held to that ping's deadline: it
keeps the same bound, and a host that delays the pong only ages it. Pings re-date a book for at
most 5 s after the newest ping answered when it was read, which went out before the venue sent the
book, so a venue whose book stream stalls while its connection still answers pings holds its leg of
the index for at most 5 s plus `max_delay` plus `max_age_ms` past its sending of the stalled book:
8 s under production's values, about 6 s when nothing delays the stream. The market prices
meanwhile only while its sources' spread, in whole basis points of their median, is within
`max_divergence_bps`, so with the two sources BTC-USD and ETH-USD take, the stalled leg moves the
index at most about 13 bps from the venue still quoting (half their 25 bps, plus the rounding to
whole basis points), and past that the market has no price. This rests on those venues writing a
pong behind what they sent before reading its ping, as they were measured to; Massive's and
TraderMade's connections, which were not measured, re-date nothing. A
production market needs two sources fresh, so a delay past the bound on either leaves it without a
price. Every part of this rests on the monotonic clock's rate, as do
the Roughtime round trips that place trusted time and with them `time.max_drift`. The enclave image
launches with Secure TSC: the guest's TSC is the processor's, scaled to the frequency the host asks
for at launch by a factor the SEV-SNP firmware sets and the host cannot change afterwards, and the
guest kernel reads that frequency from the hardware. A node refuses to start when the hardware
reports the guest TSC's frequency as under 1000 MHz, where the firmware has rounded the host's
request below what the hardware counts in and the kernel calibrates against clocks the host
emulates; at 1000 MHz or more the whole-MHz reading rounds the true frequency by at most 0.1%. The
launch measurement covers the setting, and a verifier refuses a release whose stated
`guest_features` lack it; the launch digest ties the statement to the launch, reproduced by
rebuilding `measurement.json`.
The kernel times on that TSC: kvm-clock, whose scale the host writes, is off; nothing the host
supplies after boot can mark the TSC unstable, since the kernel has no frequency scaling and no
ACPI processor driver; and a node with real attestation refuses to start when the host has had the
kernel mark it unstable at boot, which moves the kernel to a clock the host emulates
(`image/README.md`, "The guest's clock").
The
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
