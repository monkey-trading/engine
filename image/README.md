# Enclave image

The node ships as a confidential VM image for bare-metal AMD SEV-SNP hosts: a Linux kernel, an
initramfs holding the `engine` binary, and the OVMF firmware that boots them. Anyone can rebuild
the image from a tagged commit and get the same bytes, and therefore the same SEV-SNP launch
measurement. The node's attestation binds that measurement and a bundle of its enclave address,
the SHA-256 of the config file it runs with, and the hash of its TLS key.

Not yet run on SEV-SNP hardware: booting the image in a confidential VM, the hardware quote, and
the launch measurement of the firmware on a real host are unverified, and are validated in #108.

## Build

```bash
# The production image from HEAD (Docker with buildx; non-x86 machines emulate amd64, slowly)
scripts/build-image.sh out/image

# A tagged release, or the dev variant, with no build cache
scripts/build-image.sh --ref v0.1.0 --no-cache out/v0.1.0
scripts/build-image.sh --variant dev out/image-dev
```

The script builds `git archive` of the commit, so uncommitted changes never reach the image, and
sets `SOURCE_DATE_EPOCH` to the commit time. It writes:

- `vmlinuz` — Linux 6.18 LTS from `x86_64_defconfig` plus [kernel.config](kernel.config): SEV-SNP
  guest and configfs-tsm built in, no loadable modules, no SysRq or KGDB, no CPU frequency
  scaling, and no drivers for devices the image does not use (USB, ATA, SCSI, sound, graphics,
  other NICs, PCI hotplug, virtio console and RNG, keyboard, mouse, and virtual terminals)
- `initramfs.cpio.gz` — [init](init), busybox, and the static `engine` binary
- `OVMF.fd` — edk2's `AmdSevX64` firmware, which checks the kernel, initramfs, and command line
  against the hashes QEMU places in the launch, so the measurement covers all three
- `measurement.json` — the SEV-SNP launch digest, with the guest shape it assumes (vCPU count and
  type, guest features, VMM, and kernel command line) and the guest policy a verifier requires;
  the guest features are `0x201`, which turns on Secure TSC ("The guest's clock" below)
- `SHA256SUMS` — the SHA-256 of the four files above

Every input is pinned in the [Dockerfile](Dockerfile): base images by digest, Debian packages by a
snapshot date, the kernel tarball and the Rust musl standard library by SHA-256, edk2 by commit,
Rust crates by `Cargo.lock` (`--locked`), and `sev-snp-measure` with its dependencies by hash in
[requirements.txt](requirements.txt). Every download runs under [retry](retry), which runs the
same pinned download up to five times, waiting about four minutes in all between attempts, so a
brief outage of the Debian snapshot archive or another source does not fail the build;
`scripts/check-image-retry.sh` scans the Dockerfile so every download runs under [retry](retry)
and the cargo build stays `--frozen`, checks the retry helper itself, and exercises the apt
settings in [apt.conf](apt.conf) against a server that keeps answering 503. Built-in mutation
checks confirm the scan still catches a download later edited to drop its `retry` — however
disguised with environment assignments, `env`, command options, or a comment in a continuation —
and that benign commands are not flagged. CI
(`.github/workflows/ci.yaml`) runs that check, builds the image twice on separate runners for every
push to `main` and every pull request that changes its inputs, and fails if the two `SHA256SUMS`
differ.

## Production builds

The `production` cargo feature of `engine-node` makes `NodeConfig::load` refuse `mock` for
`tee.provider`, `custody.provider`, and `feeds.provider`, and a config without `[api.tls.acme]`,
whose API would serve only its self-signed certificate, whatever the rest of the config says.
The image's default `production` variant builds with it. The `dev` variant leaves it off and has a
different measurement; it runs the image on mock custody and prices, without a chain or live venues.
It launches with the same guest features and kernel command line, so on SEV-SNP it needs Secure TSC
too.

## What the guest exposes

The engine takes input from the host through seven paths:

- the API port, `node.listen` in the config, served over TLS with a key generated in the enclave,
  which also answers the CA's TLS-ALPN-01 validations for `[api.tls.acme]`, so the host forwards
  port 443 to it;
- the handover port, `tee.sealing.handover.listen`, where the next release asks for the enclave's
  keys; the request and the answer are attested and the keys encrypted, so the host only carries
  them;
- the delivery port, `tee.sealing.secrets.listen`, where the operator delivers the node's secrets;
  the offer and the receipt are attested and the bundle encrypted and signed, so the host only
  carries them;
- the metrics port, `metrics.listen`, which serves `GET /metrics` over plain HTTP and is reachable
  from the host only when the config puts it on a guest address rather than loopback;
- its outbound network: venue feeds over TLS, the custody RPC providers and the checkpoint RPC
  endpoint over HTTPS, the ACME CA's directory over HTTPS, alert webhooks, Roughtime replies it
  verifies under each server's key, and
  the DNS answers that pick their addresses, from the resolvers DHCP names;
- the wall clock, which stamps every sequenced command, bounded by the signed Roughtime readings.
  The monotonic clock that times those readings, the age of a sample, the pace of sequenced time,
  and the pings that bound how far behind a venue's stream is runs on the guest's TSC, which the
  host cannot scale ("The guest's clock" below);
- the data disk, an ext4 filesystem on the first virtio disk, mounted at `/data` with `nosuid`,
  `nodev`, and `noexec`. It holds `config.toml`, read once at boot, the log, the account history
  index, the alert outbox, the custody deposit cursor, and the sealed keys, which `node.log_dir`,
  `history.dir`, `alerts.outbox.dir`, `custody.evm.cursor_path`, and `tee.sealing.dir` must name
  under `/data`: the root filesystem and `/dev` are read-only, so a path outside `/data` fails at
  boot. The keys are sealed through `/dev/sev-guest` to this release's launch measurement and policy
  on this chip, so the host holds only ciphertext (see `crates/node/README.md`, "Enclave keys").
  Two files on the disk are the operator's one-off choices, which [init](init) removes before the
  engine starts: `/data/new-key` seals new keys for a release that has none, and
  `/data/predecessor` holds the `host:port` of the running release to take them from.

The guest kernel also takes input from the host that the launch measurement does not cover, and
nothing yet bounds what a crafted input can do to it: the ext4 filesystem on the data disk, which
the host builds; the ACPI tables QEMU supplies, whose AML the kernel runs; the emulated virtio
devices, serial port, and real-time clock; and the DHCP replies that configure the network.
Authenticating the data disk and measuring the ACPI tables are open work.

The guest also writes the engine's logs to the serial console. It runs no login or other service:
[init](init) mounts the filesystems, runs the engine, and when the engine exits, syncs the disk
and powers the guest off. The engine syncs the log to the disk before it acknowledges a command,
so stopping the VM loses nothing a client was told. The guest ignores an ACPI shutdown request,
so every stop is a hard stop: the engine does not drain its queue or write the final checkpoint
it writes on SIGTERM, and commands after the last checkpoint wait for the next one to be signed.

## The guest's clock

Every bound the node keeps in monotonic time rests on that clock's rate: a price's age, the pace of
sequenced time, the age of a trusted time sample, the Roughtime round trips that place it, and
through them `time.max_drift` (`SECURITY.md`). The image keeps that rate out of the host's hands in
three parts, none of them yet run on SEV-SNP hardware (#108):

- **Secure TSC.** Every vCPU launches with SEV_FEATURES `0x201`: SNPActive (bit 0) and SecureTSC
  (bit 9). The guest's RDTSC, RDTSCP, and TSC MSR then read the processor's TSC scaled by the
  GUEST_TSC_SCALE in the guest's encrypted VMSA, plus its GUEST_TSC_OFFSET, and the processor's own
  TSC ratio and the hypervisor's TSC offset do not apply (AMD64 APM vol. 2, 15.36.18). The firmware
  sets the scale of each vCPU it launches from the TSC frequency the host asks for, and the kernel
  takes the values for the vCPUs it starts itself from the firmware's TSC_INFO answer (SEV-SNP
  firmware ABI, 7.9 and 8.18.2.2). The kernel reads the guest TSC's frequency from the
  GUEST_TSC_FREQ MSR, corrected by the TSC_FACTOR the firmware writes in the secrets page, and stops
  the guest if the host intercepts that read or RDTSC (Linux `arch/x86/coco/sev/`). So the host
  picks the frequency at launch, but cannot make the guest count in another or change it afterwards.
- **The kernel times on the TSC.** With Secure TSC the kernel marks the TSC reliable, so it never
  checks the TSC against another clock or across vCPUs. `no-kvmclock` on the measured command line
  keeps out kvm-clock, whose scale sits in memory the host writes and which outranks the TSC
  whenever the host's CPUID hides the invariant TSC. The kernel has no CPU frequency scaling and no
  ACPI processor driver ([kernel.config](kernel.config)), so nothing the host supplies can mark the
  TSC unstable after the TSC clocksource has registered: no frequency change, and no ACPI C-state
  the TSC stops in.
- **The node checks.** At boot the host can still have the kernel mark the TSC unstable, through
  CPUID leaves naming another hypervisor (the firmware checks only the standard and extended
  leaves) or firmware tables; the kernel would then time on the HPET or the ACPI timer, which QEMU
  emulates. And at launch the host picks the frequency the firmware scales the TSC to, and can ask
  for one so low the hardware cannot count in it. So a node with real attestation
  (`tee.provider = "configfs_tsm"`) reads the clocksource
  (`/sys/devices/system/clocksource/clocksource0/current_clocksource`) and the GUEST_TSC_FREQ MSR
  before it opens its keys, and refuses to start with the Critical `clocksource_unprotected`
  unless the kernel times on the TSC in a frequency the hardware reports as 1000 MHz or more:
  under that the firmware has rounded the host's request below what the hardware counts in, and
  the kernel calibrates the TSC against clocks the host emulates. The 0.1% a whole-MHz reading can
  round a frequency by at 1000 MHz or more is the most a bound resting on the clock's rate gives
  up; it rests on the APM's statement that the MSR reports the guest TSC's effective frequency,
  confirmed only on SEV-SNP hardware (#108).

The attestation report carries no SEV features of its own (ABI, Table 27), but the launch
measurement covers each vCPU's VMSA, SEV_FEATURES included; the firmware measures the VMSA as if its
TSC scale and offset were zero, since it sets them afterwards (ABI, 8.18.2.2). A launch without
Secure TSC therefore measures to another digest, and a verifier requires the release's
`guest_features` to include both bits (see below).

The `dev` variant launches with the same features and command line. Under plain QEMU, as
[scripts/check-image.sh](../scripts/check-image.sh) boots it, there is no SEV-SNP and so no Secure
TSC: the guest times on whatever clock QEMU gives it, which the check does not rely on, and its
mock TEE skips the node's check.

## Attestation

`GET /v1/attestation` returns the quote, its 64 bytes of report data, and the attestation bundle the
report data commits to. Report data bytes 0..32 are the SHA-256 of the bundle, and bytes 32..64 are
reserved as zero. The bundle is 105 bytes:

| Bytes | Content |
| --- | --- |
| 0..21 | the ASCII domain tag `engine.attestation.v1`, which names this layout |
| 21..41 | the enclave address that signs checkpoints and withdrawals |
| 41..73 | SHA-256 of the config file, the value `sha256sum config.toml` prints |
| 73..105 | SHA-256 of the API's TLS certificate's DER SubjectPublicKeyInfo, which clients pin |

On SEV-SNP the quote is the raw attestation report. `engine verify-attestation --attestation
<v1/attestation JSON> --release <release manifest> --processor <name>` accepts it only when all
of these hold:

- its signature verifies against the VCEK certificate chain from AMD's key distribution service up
  to AMD's root key; without this, the fields below prove nothing;
- the release's `guest_features`, `measurement.json`'s, include SNPActive and SecureTSC (`0x201`);
  the report does not carry them, but the launch digest it is matched against below covers them;
- the guest policy at offset `0x08` (8 bytes, little-endian) equals `measurement.json`'s `policy`,
  which keeps debug and the migration agent off; the launch digest does not cover the policy;
- the reported TCB (firmware and microcode security versions) is at or above the minimum the
  verifier requires, so a host on firmware with known flaws is refused;
- the launch measurement at `0x90` equals `measurement.json`'s `launch_digest`;
- the report data at `0x50` is the SHA-256 of the served bundle followed by 32 zero bytes, and the
  bundle names a config file the release lists.

The verifier checks the report signature against the VCEK, fetched from the vendor's
key-distribution service by the chip id and reported TCB the quote carries, and the VCEK's chain
against the vendor roots pinned in the binary. A mock quote never verifies.

A client that may call only the node's origin, such as a browser, cannot reach the vendor's
key-distribution service. The node relays what the verifier would fetch at `GET
/v1/attestation/collateral`: `vcek`, `intermediate`, `root`, and `crl` as `0x` hex DER, and
`fetched_at` in Unix milliseconds by the host's clock (informational). The route answers 404 for a
quote with no collateral (a mock quote) and 503 until the first fetch succeeds. `engine
verify-attestation --collateral <file or URL>` verifies from it with no call to the vendor.

The relay adds no trust. A verifier takes none of it on the node's word: it requires the root to be
one pinned in the verifier's own binary, each certificate to be signed by its parent and in date,
the VCEK's signed extensions to name the quote's chip id and reported TCB, the revocation list to be
the root's, current, and not to list the intermediate, and the report's signature to verify against
the VCEK. A node, or a host between the node and the vendor, that serves other bytes can make a
verifier refuse; it cannot make one accept a chain the vendor did not issue. It can delay a newer
revocation list: a served list stays valid until its `nextUpdate`, so while the node's fetches are
blocked, or its clock reads a time that makes it refuse the fresh list, a verifier keeps accepting
the chain until the served list expires. Once a fetch succeeds, a revocation of the intermediate
reaches verifiers at once: the node serves a list that revokes its own intermediate, so verifiers
refuse the chain, and raises the Critical `collateral_revoked`.

The node treats what it fetches from the vendor as untrusted input. The service URL and processor
name come from `tee.kds_base` and `tee.processor`, so the attestation binds them through the config
hash. The first fetch runs in the background at boot, so the API serves the attestation while it is
pending, then `tee.collateral_refresh` (1 minute to 24 hours, default 12 hours) repeats it. Each
fetch is bounded: 15 seconds and three attempts per request, no redirect or proxy, TLS to the
compiled-in public roots, at most 1 MiB per answer, then at most 16 KiB per certificate and 256 KiB
for the list, and five minutes for the whole refresh. A fetch whose result is malformed, does not
link, does not name this quote's chip and TCB, or carries a list that is expired or that the root did
not sign is dropped, and the node keeps serving the last good copy; it does not pin the root, which
is the verifier's job. The one exception is a list the root signed that names the intermediate: the
node serves it, whatever its dates, because hiding vendor data that makes a verifier refuse this
node is the one failure the relay must not have. The dates in the other checks come from the host's
clock, so a skewed clock can make the node refuse a fresh candidate and keep the last copy, which
delays a revocation but never makes a verifier accept anything, since a verifier re-checks every
date on its own clock. A blocked or slowed network, or a host that tampers with the vendor's
answers, therefore costs a stale copy and a delayed revocation, never an accepted forgery. After a good fetch the node
waits `tee.collateral_refresh`, or half the time left to the list's `nextUpdate` (at least five
minutes) when that is sooner, so a replacement is fetched before the served list expires. Once the
served list is within a day of its `nextUpdate` and a refresh fails, the node raises the Warning
`collateral_stale`, and the same when a fetch fails after the node has gone a refresh interval (at
most 30 minutes) without any collateral. The Critical `collateral_revoked` fires when a refresh
fetches an authentic list that revokes the intermediate and resolves if a later list does not. The node's fetches are its only calls to the vendor, so a
deployment must allow outbound HTTPS from the enclave to `tee.kds_base`.

The route is unauthenticated and carries no per-account data. `engine replay
--attestation <JSON> --release <manifest> --processor <name>` runs the same verification and requires every
checkpoint signer to be the enclave address the report data binds; the commands below only read
the fields.

## Boot on an SEV-SNP host

Untested on hardware. The host needs SEV-SNP enabled in firmware, a processor that reports Secure
TSC (CPUID Fn8000_001F_EAX bit 8), a KVM host kernel that offers it to SNP guests (Linux 6.18's
does), and a QEMU whose `sev-snp-guest` object has the `secure-tsc` property: it is on QEMU's
`master` branch (commit `a26f9b975d5c`) and in no release up to 11.1.3. KVM sets the guest's TSC
frequency to the host's unless `tsc-frequency` names another. The launch flags must match
`measurement.json`, and `cbitpos` is 51 on Milan and later. The guest kernel refuses virtio devices
without `iommu_platform=on`.

```bash
# The data disk: a config whose node.listen is 0.0.0.0:8080, tee.provider is "configfs_tsm",
# tee.processor names the host's CPU generation (for example "Genoa"; the node refuses to start
# without it, since it relays that generation's vendor collateral), node.log_dir, history.dir,
# alerts.outbox.dir, and [tee.sealing] dir are under /data, and, for the production variant,
# [api.tls.acme] names the API's domain. The CA validates it on port 443, so forward the host's 443
# to the guest's 8080 too (hostfwd=tcp::443-:8080, where QEMU may bind it). The dev variant accepts
# mock custody and feeds, and no [api.tls.acme]. The first boot seals new keys, so it carries the
# /data/new-key marker.
mkdir disk && cp node.toml disk/config.toml && touch disk/new-key
mke2fs -t ext4 -d disk data.img 1G

qemu-system-x86_64 -enable-kvm -cpu EPYC-v4 -smp 4 -m 4G -nographic -no-reboot \
  -machine q35,confidential-guest-support=sev0 \
  -object sev-snp-guest,id=sev0,policy=0x30000,cbitpos=51,reduced-phys-bits=1,kernel-hashes=on,secure-tsc=on \
  -bios OVMF.fd -kernel vmlinuz -initrd initramfs.cpio.gz \
  -append "$(jq -r .cmdline measurement.json)" \
  -drive if=none,id=data,format=raw,file=data.img \
  -device virtio-blk-pci,drive=data,disable-legacy=on,iommu_platform=on \
  -netdev user,id=net0,hostfwd=tcp::8080-:8080 \
  -device virtio-net-pci,netdev=net0,disable-legacy=on,iommu_platform=on,romfile=

# The policy, launch digest, and report data the quote carries (its signature is not checked here)
curl -sk https://localhost:8080/v1/attestation > attestation.json
jq -r .quote attestation.json | cut -c3- | xxd -r -p > report.bin
xxd -p -s 0x08 -l 8 report.bin                 # 0000030000000000: policy 0x30000, little-endian
xxd -p -s 0x90 -l 48 report.bin | tr -d '\n'   # equals measurement.json's launch_digest
xxd -p -s 0x50 -l 64 report.bin | tr -d '\n'   # equals report_data, without 0x
jq -r .bundle attestation.json | cut -c3- | xxd -r -p | sha256sum   # report data bytes 0..32
```

[scripts/check-image.sh](../scripts/check-image.sh) runs the checks that need no hardware: it boots
the image under plain QEMU with `config/dev.toml`, and expects a production image to refuse the
mock providers and power off, or a dev image to serve an attestation whose bundle hashes to its
report data and names the config file, to serve TLS under the key the bundle names, and to refuse a
log directory, with the alert outbox beside it, outside `/data`. It boots QEMU's default firmware: the `AmdSevX64` OVMF loads a
kernel only in an SEV-SNP launch (under plain QEMU it finds no bootable option), so booting through
`OVMF.fd` is part of the hardware validation.

## Release

1. Tag `v<version>` on a `main` commit whose CI run passed. That run built the commit's image and
   its browser verifier twice each and found the builds identical; its `image-first` artifact holds
   the image files and its `wasm-verifier-first` artifact the verifier package. Artifacts expire
   after 14 days; after that, run CI on the tag (`gh workflow run ci.yaml --ref v<version>`) and
   take the files from that run.
2. Rebuild the tag on a second machine with
   `scripts/build-image.sh --ref v<version> --no-cache <dir>` and check its `SHA256SUMS` matches
   the CI run's.
3. Hash every config file the release runs with: `sha256sum <config>.toml`.
4. Create the GitHub release for the tag with `vmlinuz`, `initramfs.cpio.gz`, `OVMF.fd`,
   `measurement.json`, `SHA256SUMS`, and each config file, listed with its SHA-256, plus a
   `release.toml` manifest tying them together for `engine verify-attestation`:

   ```toml
   name = "v0.1.0" # the release this manifest verifies
   launch_digest = "0x…"        # measurement.json's launch_digest
   policy = "0x30000"           # measurement.json's policy
   guest_features = "0x201"     # measurement.json's guest_features: SNPActive and SecureTSC

   [minimum_tcb] # the lowest reported TCB a quote from this release is accepted at
   boot_loader = 0
   tee = 0
   snp = 8
   microcode = 115

   [config_hashes] # every configuration file the release runs with
   "node.toml" = "0x…"
   ```

   A verifier checks a node's quote against the release's launch digest, its policy, one of its
   config hashes, and reads that config to see what the node runs with; it refuses a release whose
   guest features lack Secure TSC. A processor name the key-distribution service knows (the CPU
   generation's codename, for example `Milan`, `Genoa`, or `Turin`) passes as `--processor`.
5. Attach the browser verifier from the same run, the files `scripts/build-wasm-verifier.sh` wrote:
   `engine_verify_bg.wasm`, its glue `engine_verify.js`, the TypeScript types `engine_verify.d.ts`
   and `engine_verify_bg.wasm.d.ts`, and `package.json`, with their `SHA256SUMS` attached as
   `engine-verify.SHA256SUMS`. On a second x86_64 Linux machine, `scripts/build-wasm-verifier.sh
   <dir>` from a checkout of the tag must give the same sums.
6. Before the node release goes live, ship a console build that pins this `release.toml`, beside
   the previous release's for the upgrade, and the verifier's SHA-256. The attestation binds the
   config file, so a console that does not pin the new manifest refuses the new node.
7. Have the deployment's multisig schedule, through its timelock, the Vault's
   `setCurrentRelease(<launch_digest>, <config_sha256>)` with `measurement.json`'s 48-byte
   `launch_digest` and the SHA-256 of the `node.toml` the new release boots with, and execute it
   once the timelock's delay has passed. The running release reads it at custody finality before it
   hands the keys over, so wait until the block that executed it is finalized. From then on
   the running release keeps sequencing but would refuse its own keys on a restart. To call the
   upgrade off, have the timelock set the running release back as current; that starts another
   generation, so the running release then hands its keys to a fresh boot of itself through step 8.
8. Boot the new release's VM with the running release's `tee.sealing.handover.listen` address in
   `/data/predecessor` (or, when an election names it, with no flag at all: it then resolves the
   incumbent's endpoint from the registry itself, fetches the published log from the incumbent's
   registered hostname or the mirrors `tee.sealing.handover.mirrors` names, and takes the keys with
   no `--predecessor`). The running release checks the new one's quote, stops sequencing, seals the
   log's tail, and hands over the keys and every log segment past the new release's tip; the new
   release writes them durably, requires the log to reach the handed-over final checkpoint, and
   only then confirms, so the running release powers off only with the whole log transferred. A
   failed transfer — the archive unreachable, a segment over the 64 MiB bound — refuses before the
   confirmation: the running release stays stopped and serves the next attempt, and the new release
   retries its start; never stop the running release, whose own copy of the keys no longer opens.
   If the new release reports `handover_unconfirmed`, start it again only once the running release
   has powered off.
9. The old release's sealed copy of the keys stays on its disk but never opens again: a node
   checks the Vault's current release and its generation before it unseals, and a release the
   timelock names again takes the keys afresh through steps 7 and 8. An upgrade that fixes a flaw in
   the old release still rotates the signer: the old release has held the key.

A config change is a new release too: the same image under another `node.toml` has another release
id, so it takes the keys through steps 7 and 8 with the new config's hash.

A first deployment deploys the Vault with a placeholder signer, has the timelock make the release
and its config current, and boots with `/data/new-key`: the node seals new keys and, until the
Vault names its address, serves only `/v1/attestation`, which governance verifies before having
the timelock call `setSigner`. The same applies after a compromise, once the old sealed file is
deleted.

The attestation binds every byte of the config file, so a verifier can check the hash only against
a copy it can read, and a credential in the file would stop that config being published. The config
therefore names its secrets, and the operator delivers them into the attested enclave on
`[tee.sealing.secrets]`'s listener with `engine admin deliver-secrets`, which checks the enclave's
quote against this release's manifest first (`crates/node/README.md`, Delivered secrets). The
guest's only environment is the measured kernel command line, so a production build refuses a secret
read from an environment variable. The host forwards that listener's port only from the operator's
addresses; it carries only encrypted bundles. A first deployment holds on that listener until the
operator delivers, before step one of the key lifecycle.

The commit time enters the kernel and every initramfs entry, so each release has its own launch
digest, even one that changes only documentation: a verifier takes the digest from that release's
`measurement.json`, never from an earlier one.
