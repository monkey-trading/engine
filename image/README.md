# Enclave image

The reproducible confidential virtual machine image in which the trading server runs on an AMD
SEV-SNP host.

The image consists of a Linux kernel, an initramfs that holds the static `engine` binary, and the
OVMF firmware that boots them. Anyone can rebuild the image from a tagged commit and obtain the same
bytes, and therefore the same SEV-SNP launch measurement. The server's attestation binds that
measurement to a bundle of three values: the server's enclave address, the SHA-256 of its
configuration file, and the hash of its TLS key.

**The image has not yet run on SEV-SNP hardware.** Booting it in a confidential VM, the hardware's
attestation report and the firmware's launch measurement on a real host are not yet verified.

## Build

You need Docker with buildx. On a machine that is not x86_64, Docker emulates amd64, and the build
is much slower.

```shell
scripts/build-image.sh out/image
```

The script builds the production image from the commit at `HEAD`. It builds from `git archive` of
that commit, so uncommitted changes never reach the image, and it sets `SOURCE_DATE_EPOCH` to the
commit's time. `--ref <tag or commit>` builds another commit, `--variant dev` builds the
development variant, and `--no-cache` disables Docker's build cache. The script writes these files
into the output directory:

| File | Contents |
| --- | --- |
| `vmlinuz` | Linux 6.18 LTS, built from `x86_64_defconfig` plus [kernel.config](kernel.config). It has the SEV-SNP guest support and configfs-tsm built in, and no loadable modules, CPU frequency scaling, debugger or drivers for devices the image does not use. |
| `initramfs.cpio.gz` | [init](init), busybox and the static `engine` binary |
| `OVMF.fd` | The `AmdSevX64` firmware, which checks the kernel, initramfs and command line against hashes in the launch, so that the measurement covers all three |
| `measurement.json` | The SEV-SNP launch digest, the guest shape it assumes (vCPU count and type, guest features, VMM and kernel command line), and the guest policy a verifier requires |
| `SHA256SUMS` | The SHA-256 of the four files above |

The [Dockerfile](Dockerfile) pins every input. Base images are pinned by digest, Debian packages by
a snapshot date, the kernel source and the Rust musl standard library by SHA-256, edk2 by commit,
Rust crates by `Cargo.lock`, and `sev-snp-measure` by hash in [requirements.txt](requirements.txt).
Every download runs under [retry](retry), which repeats it up to five times over about four minutes,
so a brief outage of a source does not fail the build. `scripts/check-image-retry.sh` checks that
every download in the Dockerfile runs under `retry` and that the Rust build stays `--frozen`. CI
builds the image twice on separate runners and fails if the two `SHA256SUMS` files differ.

The commit time enters the kernel and every initramfs entry, so every release has its own launch
digest, even one that changes only documentation. A verifier therefore takes the digest from that
release's own `measurement.json`.

## Variants

The default `production` variant builds `engine` with the `production` feature. That build refuses
a simulated enclave, custody or price feed, and an API without a certificate from an ACME authority,
whatever the configuration says. The `dev` variant leaves the feature off, so it can run on
simulated custody and prices without a chain or live venues. The `dev` variant has a different
measurement, but it launches with the same guest features and kernel command line.

[The enclave guest](../docs/enclave.md) describes what the guest takes from the host and how it
keeps the host from altering the rate of its clock.

## Attestation

`GET /v1/attestation` returns the attestation report, its 64 bytes of report data, and the bundle
that the report data commits to. Bytes 0 to 32 of the report data are the SHA-256 of the bundle,
and bytes 32 to 64 are zero. The bundle is 105 bytes long:

| Bytes | Content |
| --- | --- |
| 0..21 | The ASCII domain tag `engine.attestation.v1`, which names this layout |
| 21..41 | The enclave address that signs checkpoints and withdrawals |
| 41..73 | The SHA-256 of the configuration file, as `sha256sum config.toml` prints it |
| 73..105 | The SHA-256 of the DER SubjectPublicKeyInfo of the API's TLS certificate, which clients pin |

`engine verify-attestation` accepts a report only when all of the following hold:

- the report's signature verifies against the chip's certificate (the VCEK), and that certificate's
  chain leads to an AMD root compiled into the binary;
- the release's `guest_features` include SNPActive and SecureTSC;
- the guest policy at offset `0x08` equals the release's `policy`, which keeps debugging and the
  migration agent disabled, and which the launch measurement does not cover;
- the reported firmware and microcode security versions are at or above the release's minimum;
- the launch measurement at offset `0x90` equals the release's `launch_digest`;
- the report data at offset `0x50` is the SHA-256 of the served bundle followed by 32 zero bytes,
  and the bundle names a configuration file that the release lists.

A simulated report never verifies. A client that may call only the server's origin, such as a web
page, cannot reach AMD's key distribution service. The server therefore relays the chip's
certificate chain and revocation list at `GET /v1/attestation/collateral`, and
`engine verify-attestation --collateral <file or URL>` verifies from them. The relay adds no
trust, because the verifier checks every certificate up to the root it pins, and every date on its
own clock. A server that relays other bytes can make a verifier refuse, but cannot make it accept
a chain that AMD did not issue. The documentation of `crates/node/src/verify/refresh.rs` gives the
relay's bounds and its refresh schedule.

## Boot on an SEV-SNP host

**These steps have not yet been run on hardware.** The host needs:

- SEV-SNP enabled in its firmware, on a processor that reports Secure TSC (CPUID Fn8000_001F_EAX
  bit 8);
- a KVM host kernel that offers Secure TSC to SNP guests, such as Linux 6.18;
- a QEMU whose `sev-snp-guest` object has the `secure-tsc` property, which is on QEMU's `master`
  branch from commit `a26f9b975d5c` and in no release up to 11.1.3.

The launch flags must match `measurement.json`. `cbitpos` is 51 on Milan and later processors, and
KVM sets the guest's TSC frequency to the host's unless `tsc-frequency` names another. The guest
kernel refuses virtio devices without `iommu_platform=on`.

The data disk holds the configuration as `config.toml`. In that configuration:

- `node.listen` is `0.0.0.0:8080` and `tee.provider` is `"configfs_tsm"`;
- `tee.processor` names the host's processor generation, such as `"Genoa"`, and the server refuses
  to start without it;
- `node.log_dir`, `history.dir`, `alerts.outbox.dir`, `custody.evm.cursor_path` and
  `tee.sealing.dir` are under `/data`;
- for the production variant, `[api.tls.acme]` names the API's domain, and the host forwards its
  port 443 to the guest's port 8080 for the authority's validation. The `dev` variant accepts
  simulated custody and feeds and needs no `[api.tls.acme]`.

The first boot seals new keys, so its disk carries `/data/new-key`:

```shell
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
```

The following commands read the policy, launch digest and report data from the server's report,
without checking its signature:

```shell
curl -sk https://localhost:8080/v1/attestation > attestation.json
jq -r .quote attestation.json | cut -c3- | xxd -r -p > report.bin
xxd -p -s 0x08 -l 8 report.bin
xxd -p -s 0x90 -l 48 report.bin | tr -d '\n'
xxd -p -s 0x50 -l 64 report.bin | tr -d '\n'
jq -r .bundle attestation.json | cut -c3- | xxd -r -p | sha256sum
```

The first `xxd` prints `0000030000000000`, the policy `0x30000` in little-endian order. The second
prints the `launch_digest` from `measurement.json`, and the third prints the report data without its
`0x` prefix. The last command prints the SHA-256 of the bundle, which equals the first 32 bytes of
the report data.

`scripts/check-image.sh out/image` runs the checks that need no SEV-SNP hardware, and needs
`qemu-system-x86_64`. It boots the image under plain QEMU with `config/dev.toml` on its data disk.
A production image must refuse the simulated providers and power off. A `dev` image must serve an
attestation whose bundle matches its report data, serve TLS under the key the bundle names, and
refuse a log directory outside `/data`. The check boots QEMU's default firmware, because the
`AmdSevX64` firmware loads a kernel only in an SEV-SNP launch.

## Release

1. Tag `v<version>` on a `main` commit whose CI run passed. That run built the image and the
   browser verifier twice each and found the builds identical. Its `image-first` artifact holds the
   image files, and its `wasm-verifier-first` artifact holds the verifier package. Artifacts expire
   after 14 days, after which `gh workflow run ci.yaml --ref v<version>` produces them again.
   Before tagging, run `scripts/dev-venue-load.sh --soak` on that commit. It runs the crypto feeds
   against the live venues for 30 minutes, dropping connections, and passes only if no market is
   without a price for longer than 10 seconds.
2. Rebuild the tag on a second machine with
   `scripts/build-image.sh --ref v<version> --no-cache <dir>`, and check that its `SHA256SUMS`
   matches the CI run's.
3. Hash every configuration file the release runs with, using `sha256sum <config>.toml`.
4. Create the GitHub release for the tag with `vmlinuz`, `initramfs.cpio.gz`, `OVMF.fd`,
   `measurement.json`, `SHA256SUMS` and each configuration file, listed with its SHA-256. Add a
   `release.toml` manifest that ties them together for `engine verify-attestation`:

   ```toml
   name = "v0.1.0"              # the release this manifest verifies
   launch_digest = "0x…"        # measurement.json's launch_digest
   policy = "0x30000"           # measurement.json's policy
   guest_features = "0x201"     # measurement.json's guest_features: SNPActive and SecureTSC

   [minimum_tcb]                # the lowest reported security versions this release accepts
   boot_loader = 0
   tee = 0
   snp = 8
   microcode = 115

   [config_hashes]              # every configuration file the release runs with
   "node.toml" = "0x…"
   ```

   `--processor` takes a processor generation that AMD's key distribution service knows, such as
   `Milan`, `Genoa` or `Turin`.
5. Attach the browser verifier from the same CI run. These are the files that
   `scripts/build-wasm-verifier.sh` wrote: `engine_verify_bg.wasm`, `engine_verify.js`, the
   TypeScript types `engine_verify.d.ts` and `engine_verify_bg.wasm.d.ts`, and `package.json`.
   Attach their `SHA256SUMS` as `engine-verify.SHA256SUMS`. A build from the tag on a second
   x86_64 Linux machine must produce the same sums.
6. Before the release goes live, publish a console build that pins this `release.toml`, beside the
   previous release's, and the verifier's SHA-256. The attestation binds the configuration file, so
   a console that does not pin the new manifest refuses the new server.
7. Have the deployment's multisig schedule, through its timelock, the vault's
   `setCurrentRelease(<launch_digest>, <config_sha256>)`, with `measurement.json`'s 48-byte
   `launch_digest` and the SHA-256 of the new release's `node.toml`. Execute it once the timelock's
   delay has passed, and wait until the block that executed it is final. From then on, the running
   release keeps processing commands but would refuse its own keys after a restart. To call the
   upgrade off, have the timelock make the running release current again. That starts a new
   generation, so the running release then hands its keys to a fresh boot of itself through step 8.
8. Boot the new release's VM with the running release's `tee.sealing.handover.listen` address in
   `/data/predecessor`. When an election names the new release, leave that file out: the new release
   finds the incumbent's endpoint in the registry and fetches the published log itself. The running
   release checks the new release's report, stops processing commands, and hands over the keys and
   every log segment past the new release's last entry. The new release writes them to disk and
   confirms, and the running release then powers off. A failed transfer is refused before the
   confirmation. In that case the running release stays stopped and serves the next attempt, and
   the new release retries its start. **Never restart the running release at that point, because its
   own copy of the keys no longer opens.** If the new release reports `handover_unconfirmed`, start
   it again only once the running release has powered off.
9. The old release's sealed copy of the keys stays on its disk but never opens again, because a
   server checks the vault's current release and its generation before it unseals its keys. An
   upgrade that fixes a flaw in the old release still rotates the signing key, because the old
   release has held it.

A configuration change is also a new release. The same image under another `node.toml` has
another release identity, so it takes the keys through steps 7 and 8 with the new configuration's
hash.

A first deployment deploys the vault with a placeholder signer, has the timelock make the release
and its configuration current, and boots with `/data/new-key`. The server seals new keys and serves
only `/v1/attestation` until the vault names its address. Governance verifies that attestation
before the timelock calls `setSigner`. The same applies after a compromise, once the old sealed file
has been deleted.

The attestation binds every byte of the configuration file, so the file holds no credentials. It
names its secrets instead, and the operator delivers them into the attested enclave with
`engine admin deliver-secrets`, which first checks the enclave's report against the release's
manifest. A first deployment waits on the delivery listener until the operator delivers them.
[`ops/runbooks/key-recovery.md`](../ops/runbooks/key-recovery.md#secrets) gives the steps, and
`crates/node/src/secrets/mod.rs` documents the design.
