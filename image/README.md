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
[requirements.txt](requirements.txt). The build steps retry each of their downloads from the same
pinned source, with growing waits, for at least three minutes, so a brief outage of the Debian
snapshot archive or another source does not fail the build. CI (`.github/workflows/ci.yaml`)
builds the image twice on separate runners for every push to `main` and every pull request that
changes its inputs, and fails if the two `SHA256SUMS` differ.

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

On SEV-SNP the quote is the raw attestation report. A verifier accepts it only when all of these
hold:

- its signature verifies against the VCEK certificate chain from AMD's key distribution service up
  to AMD's root key; without this, the fields below prove nothing;
- the guest policy at offset `0x08` (8 bytes, little-endian) equals `measurement.json`'s `policy`,
  which keeps debug and the migration agent off; the launch digest does not cover the policy;
- the reported TCB (firmware and microcode security versions) is at or above the minimum the
  verifier requires, so a host on firmware with known flaws is refused;
- the launch measurement at `0x90` equals `measurement.json`'s `launch_digest`;
- the report data at `0x50` is the SHA-256 of the served bundle followed by 32 zero bytes, and the
  bundle names a config file the release lists.

The attestation verifier (#27) will implement these checks; the commands below only read the fields.

## Boot on an SEV-SNP host

Untested on hardware. The host needs SEV-SNP enabled in firmware, a KVM host kernel with SNP
support, and QEMU 9.1 or later. The launch flags must match `measurement.json`, and `cbitpos` is
51 on Milan and later. The guest kernel refuses virtio devices without `iommu_platform=on`.

```bash
# The data disk: a config whose node.listen is 0.0.0.0:8080, tee.provider is "configfs_tsm", and
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
   `measurement.json`, `SHA256SUMS`, and each config file, listed with its SHA-256. A verifier
   checks a node's quote against the release's launch digest, its policy, and one of its config
   hashes, and reads that config to see what the node runs with.

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
