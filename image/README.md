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

- the report is of version 3 to 6, names the VCEK as its signing key, and names a processor of
  CPUID family 19h (Milan, Genoa, Bergamo or Siena);
- the report's signature verifies against the chip's certificate (the VCEK), and that certificate's
  chain leads to an AMD root compiled into the binary;
- the report's CPUID family, model and stepping, the VCEK's product name, and the product line that
  AMD's root and intermediate are named for all identify one CPU model, and the release lists that
  model, with `--processor` naming its product line;
- the release's `guest_features` include SNPActive and SecureTSC;
- the guest policy at offset `0x08` equals the release's `policy`, which keeps debugging and the
  migration agent disabled, and which the launch measurement does not cover;
- the reported firmware and microcode security versions are at or above the release's minimum for
  that CPU model. AMD numbers these versions per model, so the same number on another model can
  lack the fixes that the minimum stands for;
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
- a QEMU whose `sev-snp-guest` object has the `secure-tsc` property, which commit `a26f9b975d5c`
  added. It is in QEMU's 11.2.0 release candidates and in no release up to 11.1.3.

[SEV-SNP host](../ops/runbooks/sev-snp-host.md) sets up such a host, checks it with
`scripts/snp-host-check.sh`, and launches the image with `scripts/snp-launch.sh`, which builds the
command below from `measurement.json`.

The launch flags must match `measurement.json`. `cbitpos` is 51 on Milan and later processors, and
KVM sets the guest's TSC frequency to the host's unless `tsc-frequency` names another. The guest
kernel refuses virtio devices without `iommu_platform=on`.

The guest takes two disks. The data disk, the first, holds the configuration as `config.toml`, and the log
disk, the second, is mounted at `/data/log` and holds the log alone, since the writes of the other stores
on the data disk would otherwise hold the log's syncs. Configuration keys call a running trading
server a node, as the first one below does. In that configuration:

- `node.listen` is `0.0.0.0:8080` and `tee.provider` is `"configfs_tsm"`;
- `tee.processor` names the product under which AMD's key distribution service issues the host's
  certificates (`"Milan"`, or `"Genoa"` for a Genoa, Bergamo or Siena host), and the server refuses
  to start without it and relays no certificate for another product;
- `[[tee.cpu_models]]` lists the release manifest's `cpu_models`, entry for entry. A successor, a
  standby key's vouch and a log check take a peer enclave's quote only from a model listed there,
  at that model's minimum;
- `node.log_dir` is `/data/log`, the log disk's mount point; `history.dir`, `alerts.outbox.dir`,
  `custody.evm.cursor_path` and
  `tee.sealing.dir` are under `/data`;
- for the production variant, `[api.tls.acme]` names the API's domain, and the host forwards its
  port 443 to the guest's port 8080 for the authority's validation. The `dev` variant accepts
  simulated custody and feeds and needs no `[api.tls.acme]`.

The first boot seals new keys, so its disk carries `/data/new-key`:

```shell
mkdir disk && cp node.toml disk/config.toml && touch disk/new-key
mke2fs -t ext4 -d disk data.img 1G
mke2fs -t ext4 log.img 1G

qemu-system-x86_64 -enable-kvm -cpu EPYC-v4 -smp 4 -m 4G -nographic -no-reboot \
  -machine q35,confidential-guest-support=sev0 \
  -object sev-snp-guest,id=sev0,policy=0x30000,cbitpos=51,reduced-phys-bits=1,kernel-hashes=on,secure-tsc=on \
  -bios OVMF.fd -kernel vmlinuz -initrd initramfs.cpio.gz \
  -append "$(jq -r .cmdline measurement.json)" \
  -drive if=none,id=data,format=raw,file=data.img \
  -device virtio-blk-pci,drive=data,disable-legacy=on,iommu_platform=on \
  -drive if=none,id=log,format=raw,file=log.img \
  -device virtio-blk-pci,drive=log,disable-legacy=on,iommu_platform=on \
  -netdev user,id=net0,hostfwd=tcp::8080-:8080 \
  -device virtio-net-pci,netdev=net0,disable-legacy=on,iommu_platform=on,romfile=
```

This forwards only the API's port. A node with `[tee.sealing.handover]` or `[tee.sealing.secrets]`
needs those ports to reach the guest with each peer's own address, for example over a bridged tap
device: their listeners count connections per address, and `hostfwd` shows every peer as one.

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
`qemu-system-x86_64`. It boots the image under plain QEMU with `config/dev.toml` on its data disk and an empty log disk.
A production image must refuse the simulated providers and power off. A `dev` image must serve an
attestation whose bundle matches its report data, serve TLS under the key the bundle names, and
refuse a log directory outside `/data`. The check boots QEMU's default firmware, because the
`AmdSevX64` firmware loads a kernel only in an SEV-SNP launch.

## Release

A release is the image built from a tagged commit of `main`, the browser verifier built from the
same commit, the configurations the trading server runs with, and a manifest that ties them
together. The release workflow, [release.yaml](../.github/workflows/release.yaml), builds them and
publishes them as a GitHub release. Its files are these:

| File | Contents |
| --- | --- |
| `vmlinuz`, `initramfs.cpio.gz`, `OVMF.fd`, `measurement.json`, `SHA256SUMS` | The production image, as [Build](#build) describes |
| `engine_verify_bg.wasm`, `engine_verify.js`, `engine_verify.d.ts`, `engine_verify_bg.wasm.d.ts`, `package.json` | The browser verifier's npm package, which `scripts/build-wasm-verifier.sh` builds |
| `engine-verify.SHA256SUMS` | The SHA-256 of the browser verifier's files |
| `production.toml`, `staging.toml` | The configuration of each deployment, one for each file in [config/release/](../config/release): that file followed by `config/benchmark-publishers.toml` and `config/production-genesis.toml` |
| `release.toml` | The manifest that `engine verify-attestation` checks an attestation against |

The configurations hold no credentials, only the names of the secrets that the operator delivers.
Each operator candidate's configuration is a file of its own in `config/release/`, beside
production's and staging's. Production runs on Arbitrum One. Staging is a second guest on the same
host, on Arbitrum Sepolia with contracts of its own, and runs each release for 24 hours before
production does.

The manifest takes the launch digest, policy and guest features from `measurement.json`, and the CPU
models from the production configuration's `[[tee.cpu_models]]`:

```toml
format = 2                   # the manifest format; a manifest without it is refused
name = "v0.1.0"              # the release this manifest verifies
launch_digest = "0x…"        # measurement.json's launch_digest
policy = "0x30000"           # measurement.json's policy
guest_features = "0x201"     # measurement.json's guest_features: SNPActive and SecureTSC

[[cpu_models]]               # one per CPU model the release accepts: Siena (EPYC 8004) at launch
product_name = "Genoa-A2"    # the productName its VCEK carries, as read from the host's VCEK
family = 0x19                # the CPUID family, model and stepping its reports carry
model = 0xA0
stepping = 0x02
minimum_tcb = { boot_loader = 0, tee = 0, snp = 0x1D, microcode = 0x1B } # AMD's floor for it

[config_hashes]              # the SHA-256 of each configuration the release ships
"production.toml" = "0x…"
"staging.toml" = "0x…"
```

Set each model's `minimum_tcb` in the configurations' `[[tee.cpu_models]]` from AMD's latest
security bulletins for that model only. A verifier and a peer enclave then take the same models at
the same floors, and the workflow refuses configurations whose tables differ. Accepting another CPU
model is a new release, with that model's own floor. `--processor` takes the product under which
AMD's key distribution service issues the host's certificates: `Milan`, or `Genoa` for a Genoa,
Bergamo or Siena host. Turin is not supported.

### Make a release

1. Set every value that the release's configurations still mark with `OWNER` or `REPLACE`, in
   `config/release/`. The workflow refuses a tag while any marker is left.
2. Choose a commit of `main` whose CI run passed. The run must be a push to `main`, the nightly run
   or a manual run, because only those run every job. On that commit, run
   `scripts/dev-venue-load.sh --soak`. It runs the crypto feeds against the live venues for 30
   minutes, dropping connections, and passes only if no market is without a price for longer than 10
   seconds.
3. Run the workflow as a dry run on the commit:

   ```shell
   gh workflow run release.yaml -f commit=<sha>
   ```

   `<sha>` is the commit. The dry run builds and checks every file, and publishes nothing. It leaves
   the files in the run's `release` artifact and shows the manifest and the release notes in the
   run's summary. It reports as warnings what would refuse a tag, such as a commit that is not on
   `main` or a marker left in a configuration. CI runs the same dry run on every pull request that
   changes the release's workflows or the script that writes the manifest.
4. Tag the commit `v<MAJOR>.<MINOR>.<PATCH>`, starting at `v0.1.0`, and push the tag. Each tag needs
   the owner's approval. A tag runs the release workflow of its own commit, so the workflow's checks
   bind only the tags that are allowed. The repository's tag ruleset must therefore limit `v*` tags to
   the owner's account.
5. The workflow refuses a tag that is not a `v` followed by three numbers, a commit that is not on
   `main`, and a commit without a passing CI run of the kind step 2 names. It then builds the
   production image and the browser verifier twice each, on separate runners, and stops if either
   pair differs by a byte. It writes the configurations and the manifest, gives every file a GitHub
   artifact attestation, and creates a draft release whose notes list the commits since the previous
   tag. If the workflow refused a tag because its commit's CI run had not finished, re-run the
   workflow once that CI run has passed.
6. Review the draft, and publish it.

### Verify a release

Each file's attestation shows that the release workflow built it from the tag:

```shell
gh attestation verify vmlinuz --repo monkey-trading/engine --deny-self-hosted-runners \
  --signer-workflow monkey-trading/engine/.github/workflows/release.yaml \
  --source-ref refs/tags/v<version>
```

The same command checks any other file of the release. `sha256sum -c SHA256SUMS` and
`sha256sum -c engine-verify.SHA256SUMS` check the image and the browser verifier against their sums.
A tag runs its own commit's release workflow, so also check, in a clone, that the tag's commit is on
`main`:

```shell
git fetch origin main --tags
git merge-base --is-ancestor v<version> origin/main && echo "on main"
```

The second command prints `on main` when the tag's commit is on `main`.

The attestation shows only where a file was built. Before you boot a release on a host, rebuild the
image from the tag on that host, and refuse the release if the sums differ:

```shell
scripts/build-image.sh --ref v<version> --no-cache <dir>
diff <dir>/SHA256SUMS SHA256SUMS
```

`diff` prints nothing when the builds match. A build of the browser verifier from the tag on another
x86_64 Linux machine, with `scripts/build-wasm-verifier.sh`, must produce
`engine-verify.SHA256SUMS`. Each configuration's hash in `release.toml` is that of its files at the
tag, for example:

```shell
git checkout v<version>
cat config/release/production.toml config/benchmark-publishers.toml \
  config/production-genesis.toml | sha256sum
```

### Put a release live

The steps below change the live system, and each needs its own approval.

1. Before the release goes live, publish a console build that pins this `release.toml`, beside the
   previous release's, and the verifier's SHA-256. The attestation binds the configuration file, so
   a console that does not pin the new manifest refuses the new server.
2. Have the deployment's multisig schedule, through its timelock, the vault's
   `setCurrentRelease(<launch_digest>, <config_sha256>)`, with `measurement.json`'s 48-byte
   `launch_digest` and the SHA-256 of the environment's configuration, which `release.toml` lists.
   Execute it once the timelock's delay has passed, and wait until the block that executed it is
   final. From then on, the running release keeps processing commands but would refuse its own keys
   after a restart. To call the upgrade off, have the timelock make the running release current
   again. That starts a new generation, so the running release then hands its keys to a fresh boot
   of itself through step 3.
3. Boot the new release's VM with the running release's `tee.sealing.handover.listen` address in
   `/data/predecessor`. When an election names the new release, leave that file out, because the
   new release finds the incumbent's endpoint in the registry and fetches the published log itself.
   The running release checks the new release's report, stops processing commands, and hands over
   the keys and every log segment past the new release's last entry. The new release writes them to
   disk and confirms, and the running release then powers off. A failed transfer is refused before
   the confirmation. In that case the running release stays stopped and serves the next attempt,
   and the new release retries its start. **Do not shut the running release down at that point. Its
   own copy of the keys no longer opens, so after a shutdown it could not serve the next attempt.**
   If the new release reports `handover_unconfirmed`, start it again only once the running release
   has powered off.
4. The old release's sealed copy of the keys stays on its disk but never opens again, because a
   server checks the vault's current release and its generation before it unseals its keys. An
   upgrade that fixes a flaw in the old release still rotates the signing key, because the old
   release has held it.

A configuration change is also a new release. The same image under another configuration has
another release identity, so it takes the keys through steps 2 and 3 with the new configuration's
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
