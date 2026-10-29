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
  guest and configfs-tsm built in, no loadable modules, no SysRq or KGDB, and no drivers for
  devices the image does not use (USB, ATA, SCSI, sound, graphics, other NICs, PCI hotplug,
  virtio console and RNG, keyboard, mouse, and virtual terminals)
- `initramfs.cpio.gz` — [init](init), busybox, and the static `engine` binary
- `OVMF.fd` — edk2's `AmdSevX64` firmware, which checks the kernel, initramfs, and command line
  against the hashes QEMU places in the launch, so the measurement covers all three
- `measurement.json` — the SEV-SNP launch digest, with the guest shape it assumes (vCPU count and
  type, guest features, VMM, and kernel command line) and the guest policy a verifier requires
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
`tee.provider`, `custody.provider`, and `feeds.provider`, whatever the rest of the config says.
The image's default `production` variant builds with it. The `dev` variant leaves it off and has a
different measurement; it runs the image on mock custody and prices, without a chain or live venues.

## What the guest exposes

The engine takes input from the host through five paths:

- the API port, `node.listen` in the config, served over TLS with a key generated in the enclave;
- the metrics port, `metrics.listen`, which serves `GET /metrics` over plain HTTP and is reachable
  from the host only when the config puts it on a guest address rather than loopback;
- its outbound network: venue feeds over TLS, the custody RPC providers and the checkpoint RPC
  endpoint over HTTPS, alert webhooks, Roughtime replies it verifies under each server's key, and
  the DNS answers that pick their addresses, from the resolvers DHCP names;
- the clock, which stamps every sequenced command, bounded by the signed Roughtime readings. The
  guest launches without Secure TSC (`guest_features` is `0x1` in `measurement.json`), so the host
  can still scale the monotonic clock that times those readings and the age of a sample;
- the data disk, an ext4 filesystem on the first virtio disk, mounted at `/data` with `nosuid`,
  `nodev`, and `noexec`. It holds `config.toml`, read once at boot, the log, the account history
  index, the alert outbox, and the custody deposit cursor, which `node.log_dir`, `history.dir`,
  `alerts.outbox.dir`, and `custody.evm.cursor_path` must name under `/data`: the root filesystem
  and `/dev` are read-only, so a path outside `/data` fails at boot.

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
verifier refuse; it cannot make one accept a chain the vendor did not issue. It can withhold a newer
revocation list: a served list stays valid until its `nextUpdate`, so a revocation reaches a verifier
only as fast as the node's refreshes do, and a host that blocks the node's fetches delays it until
the served list expires.

The node treats what it fetches from the vendor as untrusted input. The service URL and processor
name come from `tee.kds_base` and `tee.processor`, so the attestation binds them through the config
hash. The first fetch runs in the background at boot, so the API serves the attestation while it is
pending, then `tee.collateral_refresh` (1 minute to 24 hours, default 12 hours) repeats it. Each
fetch is bounded: 15 seconds and three attempts per request, no redirect or proxy, TLS to the
compiled-in public roots, at most 1 MiB per answer, then at most 16 KiB per certificate and 256 KiB
for the list, and five minutes for the whole refresh. A fetch whose result is malformed, does not
link, does not name this quote's chip and TCB, or lists a revoked intermediate is dropped, and the
node keeps serving the last good copy; it does not pin the root, which is the verifier's job. The
dates in those checks come from the host's clock, so a skewed clock can only make the node refuse
candidates. A blocked or slowed network, or a host that tampers with the vendor's answers, therefore
costs a stale copy and a delayed revocation, never an accepted forgery. After a good fetch the node
waits `tee.collateral_refresh`, or half the time left to the list's `nextUpdate` (at least five
minutes) when that is sooner, so a replacement is fetched before the served list expires. Once the
served list is within a day of its `nextUpdate` and a refresh fails, the node raises the Warning
`collateral_stale`, and the same when a fetch fails after the node has gone a refresh interval (at
most 30 minutes) without any collateral. The node's fetches are its only calls to the vendor, so a
deployment must allow outbound HTTPS from the enclave to `tee.kds_base`.

The route is unauthenticated and carries no per-account data. `engine replay
--attestation <JSON> --release <manifest> --processor <name>` runs the same verification and requires every
checkpoint signer to be the enclave address the report data binds; the commands below only read
the fields.

## Boot on an SEV-SNP host

Untested on hardware. The host needs SEV-SNP enabled in firmware, a KVM host kernel with SNP
support, and QEMU 9.1 or later. The launch flags must match `measurement.json`, and `cbitpos` is
51 on Milan and later. The guest kernel refuses virtio devices without `iommu_platform=on`.

```bash
# The data disk: a config whose node.listen is 0.0.0.0:8080, tee.provider is "configfs_tsm",
# tee.processor names the host's CPU generation (for example "Genoa"; the node refuses to start
# without it, since it relays that generation's vendor collateral), and
# node.log_dir, history.dir, and alerts.outbox.dir are under /data. The dev variant accepts mock
# custody and feeds.
mkdir disk && cp node.toml disk/config.toml
mke2fs -t ext4 -d disk data.img 1G

qemu-system-x86_64 -enable-kvm -cpu EPYC-v4 -smp 4 -m 4G -nographic -no-reboot \
  -machine q35,confidential-guest-support=sev0 \
  -object sev-snp-guest,id=sev0,policy=0x30000,cbitpos=51,reduced-phys-bits=1,kernel-hashes=on \
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

1. Tag `v<version>` on a `main` commit whose CI run passed. That run built the commit's image twice
   and found the builds identical; its `image-first` artifact holds the release files. Artifacts
   expire after 14 days; after that, run CI on the tag (`gh workflow run ci.yaml --ref v<version>`)
   and take the files from that run.
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

   [minimum_tcb] # the lowest reported TCB a quote from this release is accepted at
   boot_loader = 0
   tee = 0
   snp = 8
   microcode = 115

   [config_hashes] # every configuration file the release runs with
   "node.toml" = "0x…"
   ```

   A verifier checks a node's quote against the release's launch digest, its policy, one of its
   config hashes, and reads that config to see what the node runs with. A processor name the
   key-distribution service knows (the CPU generation's codename, for example `Milan`, `Genoa`,
   or `Turin`) passes as `--processor`.

The attestation binds every byte of the config file, so a verifier can check the hash only against
a copy it can read. A credential in the file, such as an API key in a `custody.evm.providers`
entry's `rpc_url`, must reach the node another way before that config can be published; until then, only whoever holds
the file can check its hash. The guest's only environment is the measured kernel command line, so
the webhook URL that `[alerts.critical]` reads from an environment variable has no way in either:
a config with real attestation needs that route, so it cannot start in the image until secrets
are released to the attested enclave.

The commit time enters the kernel and every initramfs entry, so each release has its own launch
digest, even one that changes only documentation: a verifier takes the digest from that release's
`measurement.json`, never from an earlier one.
