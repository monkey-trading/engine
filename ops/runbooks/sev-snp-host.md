# SEV-SNP host

Use this runbook to order a bare-metal AMD SEV-SNP server from OVHcloud, to set it up, and to boot
the enclave image on it as three confidential guests, one for each operator. Each guest runs the
trading server against a local chain, and you verify each guest's attestation from outside it. The
last procedure measures the trading server inside a guest and on the same host outside it, so that
the guest's vCPU count, memory and disk can be set from the figures.

Commands, configuration keys and output call a running trading server a node. A guest is one
confidential virtual machine that runs the image. The host is the bare-metal server that runs the
guests.

**No step of this runbook has yet run on the host.** A step whose outcome depends on the server is
marked **UNVERIFIED**, with the check that settles it. Record each check's result where the step
says, and change the step once the server has answered it.

- [Know before you start](#know-before-you-start)
- [Order the server](#order-the-server)
- [Plan each store's capacity](#plan-each-stores-capacity)
- [Set up the host](#set-up-the-host)
- [Build the image](#build-the-image)
- [Boot the three guests](#boot-the-three-guests)
- [Boot the production variant](#boot-the-production-variant)
- [Measure inside the guest](#measure-inside-the-guest)
- [How to tell it is done, and when to escalate](#how-to-tell-it-is-done-and-when-to-escalate)
- [Sources](#sources)

## Know before you start

- **The server is OVHcloud's Advance-5, with an AMD EPYC 8224P.** That processor is a Siena part:
  AMD's fourth EPYC generation, built on Zen 4c cores. OVHcloud lists the Advance-5 as its AMD
  server for confidential computing. Another Siena part, the EPYC 8024P, reports the SEV-SNP and
  Secure TSC CPUID bits, and AMD's key distribution service issues Siena's certificates under the
  name `Genoa`. The guest's `tee.processor` and `engine verify-attestation --processor` therefore
  take `Genoa`. **UNVERIFIED: that the EPYC 8224P reports Secure TSC,** which
  [the host check](#run-the-host-check) reads.
- **The release accepts one CPU model: Siena.** AMD numbers each firmware and microcode security
  version per CPU model, so a release lists each model it accepts with that model's own minimum,
  and refuses a quote from any other. Siena reports CPUID family 19h, model A0h and stepping 2 (the
  A2 stepping of AMD's bulletins). Its VCEK names a product, such as `Genoa-A2`, that the release
  lists beside the CPUID. **UNVERIFIED: the product name that the EPYC 8224P's VCEK carries,**
  which [Verify each guest's attestation](#verify-each-guests-attestation) reads.
- **The launch measurement does not depend on the host's processor generation.** The digest in
  `measurement.json` covers the firmware, kernel, initramfs, command line and each vCPU's state.
  That state carries the vCPU type that QEMU presents (`EPYC-v4`) and the SEV features `0x201`
  (SNPActive and SecureTSC), not the host's model. A host matters to the measurement only through
  whether it can launch those features at all, which [the host check](#run-the-host-check)
  settles.
- **A Turin server (EPYC 9005, which OVHcloud's Scale 2026 range uses) cannot be verified today.**
  `engine verify-attestation` implements the certificate layout of Milan, Genoa and Siena only, and
  refuses Turin's. Do not order a Scale 2026 server for this work.
- **No QEMU release can launch a guest with Secure TSC.** The `secure-tsc` property of QEMU's
  `sev-snp-guest` object is in the 11.2.0 release candidates, from `v11.2.0-rc0`, and in no release
  up to 11.1.3. Ubuntu 26.04 ships QEMU 10.2.1, so [Build QEMU](#build-qemu) builds it from its
  tagged source.
- **The host kernel must be Linux 6.18 or later.** KVM offers Secure TSC to SEV-SNP guests from
  6.18. Ubuntu 26.04 ships Linux 7.0, which includes that support.
- **The BIOS must enable SEV-SNP, and leave room for at least three guests.** OVHcloud gives
  access to the BIOS through the server's KVM console, but whether this board shows the SEV-SNP
  settings is **UNVERIFIED**. If it hides them, OVHcloud support must enable them.
- **The SEV firmware's version decides whether a report verifies.** AMD's guide says that EPYC 9004
  servers take SEV firmware updates through BIOS updates, which OVHcloud controls on its servers.
  Whether the Advance-5 can load a newer firmware any other way is **UNVERIFIED**. OVHcloud's own
  attestation service, in its alpha, has not yet verified a Siena server, because the one it
  measured reported a firmware version below that service's minimum. Read the version that the
  host reports in [Check SEV-SNP on the host](#check-sev-snp-on-the-host), and compare it with
  AMD's latest SEV security bulletin for the processor.
- **`engine verify-attestation` accepts attestation reports of versions 3 to 6.** Revision 1.59
  of AMD's firmware specification defines versions 2, 3, 5 and 6, and the verifier reads the
  unpublished version 4, which SEV firmware 1.55.31 issues, as version 3. From version 3 a report
  names the processor's CPUID family, model and stepping. The verifier refuses version 2, which
  names none, and every family but 19h, which covers Milan, Genoa, Bergamo and Siena. It also refuses a report signed
  with a VLEK or with the chip-secret VCEK, because only the VCEK that AMD's key distribution
  service issues can be checked. [Verify each guest's attestation](#verify-each-guests-attestation)
  prints the version and the family. A guest reads its own report before it opens its keys, so
  firmware that issues a later version stops every guest at its next start. **Before a BIOS update
  on a host that runs guests, check the SEV-SNP API version of the firmware it carries: above 1.59,
  it may issue a report version that the release refuses.**
- **A guest with real attestation reaches the chain only over HTTPS, with a certificate that a
  public authority issued.** The trading server refuses plain HTTP to any address other than its own
  loopback, and it trusts only the public roots compiled into it. The local chain therefore sits
  behind a TLS proxy on the host, under a DNS name that you control.
- **A guest with real attestation refuses the keys that the repository publishes for
  development.** The operator key is therefore a fresh key for each guest. The local screening
  source signs with a published key, so a guest runs without screening, and the trading server
  refuses the actions that screening would clear, such as orders. A guest still seals its keys,
  serves its attestation, prices its markets and signs checkpoints that the vault accepts.
- **Three operators on one host share that host's failures.** One power, network or host fault
  stops all three guests at once. That is acceptable for this work, which measures and attests.

## Order the server

The owner places the order on the deployment's own OVHcloud account. Choose these options in the
Advance-5 configurator:

- [ ] **Server:** Advance-5, AMD EPYC 8224P, 24 cores and 48 threads at 2.55 GHz (3.0 GHz boost).
- [ ] **Memory:** 192 GB. The configurator offers 96 GB to 576 GB.
- [ ] **Storage:** at least 2 × 1.92 TB NVMe. Each guest's log goes on one drive and its derived
  stores, which are the history index, the marks cache, the recovery caches and the daily snapshots,
  on the other. Their buffered writes hold the log's batch syncs when they share its disk, and a
  production build refuses a log directory on the device of any of them. The confidential computing page lists 2 × 960 GB to
  8 × 7.68 TB. Which sizes the configurator offers for this server is **UNVERIFIED**.
- [ ] **Public bandwidth:** the default 1 Gbit/s.
- [ ] **Operating system:** Ubuntu Server 26.04 LTS, without a hypervisor template.
- [ ] **Datacentre:** any that offers the Advance-5. Nothing in this runbook depends on it.
- [ ] **Term:** monthly, without a longer commitment, while the server is a test host.
- [ ] **No additional IP addresses yet.** The guests in this runbook reach the network through the
  host. Three production guests that each answer an ACME authority on port 443 need either three
  public addresses or a proxy on the host that routes by server name.
- [ ] **Before ordering, the owner may ask OVHcloud** whether the Advance-5's BIOS shows the SEV-SNP
  settings and which SEV firmware version its current BIOS carries. A "no" to the first means a
  support ticket after delivery.

These options come from the following sizing. Each figure is the trading server's own, measured on
a development machine at 100,000 accounts on the launch book.

| Need | One guest | Three guests | Where the figure comes from |
| --- | --- | --- | --- |
| vCPUs | 4 | 12 of the host's 48 threads | The guest shape that `measurement.json` measures. Under ten times launch flow the load test's process, which also holds the price stand-ins and the screening source, peaked at 4.6 cores. |
| Memory | 16 GiB (proposed) | 48 GiB | The launch bar asks for four times the trading server's resident memory. That memory is 1.2 GiB for the trading server and its stats worker's copy, and 2.6 GB to 3.6 GB for the load test's whole process. |
| Disk | See [Plan each store's capacity](#plan-each-stores-capacity) | | The soak's measured growth of each store. The history index keeps 30 days of account events, at most about 97 GB at launch flow. |

The memory figure is not covered by the launch measurement, so a guest's memory can change without a
new release. The vCPU count is covered by it, so a different count is a new image. The 192 GB
covers three 16 GiB guests, a 32 GiB benchmark guest, the same benchmarks run on the host, and the
64 GB that a full 30-day replay at 100,000 accounts is projected to need. The memory of an
SEV-SNP guest is private to it and cannot be swapped, so the guests' memory must fit in the host's.

## Plan each store's capacity

The trading server keeps six stores on disk. This section sizes each from the soak's measurements, says what
the server alerts on, and says what can and cannot be relied on in the figures it reports. **The log and the
daily snapshots grow without a bound. The history index keeps 30 days of account events and is bounded by
them.** Until each store has a bound, plan a node that must run for months on these figures.

### What each store grows by

The figures come from the soak at ten times launch flow, at 10,000 accounts and about 263 commands a second.
Launch flow's daily log of 5.7 GB, which the sizing above gives, is about 7.5
million commands a day at the soak's 715 to 800 bytes a command, or about 87 a second: the price commands
are the same at every multiple, so ten times launch flow's trading is about three times its commands. The
launch-flow rows assume the soak's bytes a command hold at the launch mix.

| Store | Per command | Launch flow, a day | Ten times launch flow, a day | Bounded |
| --- | --- | --- | --- | --- |
| Log | 715 to 800 bytes | 5.7 GB | 18 GB | No: every segment is kept, since recovery replays from genesis |
| History index | see [The history index's bound](#the-history-indexs-bound) | about 2.8 GB with `[snapshots]`, 0.6 GB without | about 22 GB with `[snapshots]`, 4.7 GB without | Yes: 30 days of events, by `history.retention` |
| Recovery caches | none | flat | flat | Yes: `snapshots.retain` of them; two were about 38 MB each at 10,000 accounts, and the sizing above allows up to 4 GiB in all at 100,000 |
| Daily snapshots | none | about 0.4 GB, estimated | about 0.4 GB, estimated | No: every day's is kept. The 420 MB is an estimate at 100,000 accounts and **has not been measured** |
| Marks cache | none | small, not measured | small, not measured | By the retentions in `[history.marks]` |
| Alert outbox | none | none | none | Yes: `alerts.outbox.max_bytes` and `max_pages` |

On the two drives, the log's own and the data drive that holds everything else:

| One 1.92 TB drive | Launch flow | Ten times launch flow |
| --- | --- | --- |
| Log drive full in | about 337 days | about 107 days |
| Data drive full in (history index and snapshots) | about 12 years | about 8 years |
| Data drive's Warning (15% free) in | about 10 years | about 6 years |
| Log drive's Warning (15% free) in | about 286 days | about 91 days |
| History index once it holds 30 days, with `[snapshots]` | about 85 GB, up to 97 GB | about 655 GB, up to 750 GB |
| History index once it holds 30 days, without `[snapshots]` | about 18 GB | about 142 GB |

The data drive's rows take the larger index, the one with `[snapshots]`, and the daily snapshots' estimate:
once the index holds 30 days, only the daily snapshots grow the drive. A drive's usable size is below its
label, since the filesystem keeps some of it, so each figure is a little optimistic. The three guests of the
test host share its two drives, so a table for one node does not hold for three.

### The history index's bound

The history index keeps the account events of the newest 30 days of sequenced time (`history.retention`), so
its size is the events it holds times what each costs on disk. The figures come from a recorded log of two
hours of ten times launch flow at 100,000 accounts: 1.85 million commands and 0.59 account events a command.
Launch flow's trading, and so its trading events, are a tenth of the recording's; the rollover's rows are the
same at both.

| Input | Launch flow | Ten times launch flow |
| --- | --- | --- |
| Account events from trading, a day | about 1.3 million | about 13 million |
| Financing rows from the daily rollover, one an open position | 437,500 | 437,500 |
| Events in 30 days | about 52 million | about 404 million |

| Index | Bytes an event held | 30 days at launch flow | 30 days at ten times launch flow |
| --- | --- | --- | --- |
| With `[snapshots]`, the authenticated index | about 1,620, up to 1,850 | about 85 GB, up to 97 GB | about 655 GB, up to 750 GB |
| Without `[snapshots]` | about 350 | about 18 GB | about 142 GB |

The production release configurations leave `[snapshots]` out while rotation is on, so a node at launch runs
the index without it. The authenticated index's figure held from 0.1 GB to 1 GB of index on a development
machine; its upper figure allows for the eighth of the file's pages that collection leaves free for later
writes. The other index's figure held as it reached 545,000 events and then stayed level while it dropped
events past its retention. The rollover's rows are taken to cost what a trading event does. Both 30-day
figures are projections from those costs: neither index has been run at that size. Collection also keeps the
versions the two newest recovery caches pin, a few minutes of writes, which these figures leave out.

A shorter `history.retention` lowers the bound in proportion. A release sets it, and a node whose index was
built under another retention discards the index and rebuilds it from the log on its first start.

### What the server alerts on

Each store's bytes and each volume's free space are metrics, read every `alerts.storage.interval` (a
minute). A volume is named by the first store on it, in the order of the list below, so adding or removing an
optional store on a volume that holds the log or the history directory leaves its name. The production layout
has the volumes `log` and `history_index`, and an alert's message lists every store on its volume with its
size. Each read lists the log's directory and reads each of its files but the sealed segments, which it reads
once, since a sealed segment never changes.

| Metric | Meaning |
| --- | --- |
| `engine_store_bytes{store}` | Bytes the disk allocates to each store's files: `log`, `history_index`, `marks_cache`, `recovery_caches`, `daily_snapshots`, `alert_outbox` |
| `engine_volume_total_bytes{volume}` | Size of each volume |
| `engine_volume_free_bytes{volume}` | Bytes available to the server on each volume |
| `engine_volume_growth_bytes_per_second{volume}` | Growth of used bytes over `alerts.storage.rate_window`, once the window is sampled |
| `engine_volume_fill_seconds{volume}` | Seconds to full at that growth, present only while the volume grows |

The server raises `disk_space_low` (Warning) and `disk_space_critical` (Critical) for a volume, and the
operator's Prometheus rules `EngineVolumeNearlyFull` and `EngineVolumeFillingSoon` repeat them from outside,
because a full volume can stall the page about itself on the alert outbox. The production defaults are in
`[alerts.storage]`, with the reasoning beside each:

| Setting | Warning | Critical |
| --- | --- | --- |
| Free share of the volume below | 15% | 5% |
| Projected to fill within | 14 days | 3 days |

The projection reads the growth over the last six hours and makes none until the node has run that long.
It follows the day's rhythm, so a quiet weekend projects a longer time than the Monday that follows. A level
clears only once its measure is a tenth past the threshold.

**The figures are the host's.** The host reports the size and free space of its disks, and its clock times
the growth, so a host can make a volume look empty or full. The alerts are for the operator, who runs the
host; the engine reads none of them, and no protocol rule, checkpoint or client answer depends on them. They
do not replace the log's own checks: a host that stalls or fills the disk is also caught by `log_sync_held`.

### When a disk alert fires

1. Name the volume from the alert, and read `engine_store_bytes` for the stores on it. The largest is the
   one that grows.
2. For the history index, the lever is `history.retention`, which a new release sets. For the daily
   snapshots, copy old ones off the host and remove them as [Daily
   snapshots](restart-and-recovery.md#daily-snapshots) step 4 describes. For the log, nothing removes
   segments: add disk, or move the data to a larger drive during a planned stop.
3. Do not delete the log's segments or a recovery cache by hand to make room. A segment missing from the log
   fails recovery, as [The trading server will not start](restart-and-recovery.md#the-trading-server-will-not-start)
   describes.
4. Tell the owner for any `disk_space_critical`, with the volume, the free space and the largest store.

## Set up the host

Run these steps on the host, over SSH as the `ubuntu` user that OVHcloud's template creates. A step
marked **Changes the host** installs software, changes the host's settings or firmware, runs a guest
or a service, or sends transactions to the local chain. Each such step needs the owner's approval
for that step, as [How the steps are marked](README.md#how-the-steps-are-marked) describes. A step
that only reads, or only writes files under `~/engine` and `~/snp`, carries no mark. The host holds
no funds, and its chain is local, so no step in this runbook changes the live system.

### Install the operating system

1. Check the kernel and its command line:

   ```shell
   uname -r
   cat /proc/cmdline
   ```

   The first command prints a kernel from 6.18 on, such as Ubuntu 26.04's `7.0.0-42-generic`. The
   command line must not contain `iommu=pt`, `iommu=off` or `amd_iommu=off`, because SEV-SNP needs
   the IOMMU to translate. If one is there, remove it from `GRUB_CMDLINE_LINUX` in
   `/etc/default/grub`, then run `sudo update-grub` and reboot. **Changes the host.**

2. Install the packages that the following steps use:

   **Changes the host.**

   ```shell
   sudo apt-get update
   sudo apt-get install -y build-essential git jq xxd cpuid cpio busybox-static musl-tools tmux \
     e2fsprogs python3 python3-venv ninja-build pkg-config libglib2.0-dev libpixman-1-dev \
     libslirp-dev docker.io docker-buildx caddy
   sudo usermod -aG docker ubuntu
   curl -sSf https://sh.rustup.rs | sh -s -- -y
   curl -L https://foundry.paradigm.xyz | bash
   ```

   Log out and back in, then run `foundryup`, which installs `forge`, `cast` and `anvil`.

3. Clone the repository at the commit to measure, fetch the contracts' dependencies, install the
   Rust toolchain, and start the file that holds the variables of the later steps:

   **Changes the host.**

   ```shell
   git clone https://github.com/monkey-trading/engine.git ~/engine
   cd ~/engine/contracts && forge soldeer install && cd ~/engine
   rustup default 1.99.1
   rustup target add x86_64-unknown-linux-musl
   mkdir -p ~/snp && echo "SNP=$HOME/snp" > ~/snp/env
   . ~/snp/env
   ```

   Rust 1.99.1 is the toolchain that `image/Dockerfile` builds the image's engine with. The later
   steps run from `~/engine` with the variables in `~/snp/env`, so start each new shell, including
   the one after the BIOS step's reboot, with `cd ~/engine && . ~/snp/env`.

### Enable SEV-SNP in the BIOS

1. In the OVHcloud Control Panel, open the server's **IPMI/KVM** tab and start the HTML KVM console.
2. Reboot into the BIOS setup. From the running host, OVHcloud's guide uses `ipmiutil reset -b`.
   Alternatively, press `DEL` in the console while the server starts.

   **Changes the host.**

3. Set the following options, which are AMD's names for them. A board vendor's BIOS can name or
   place them differently.

   **Changes the host's firmware settings.** The owner approves this step.

   | Menu | Option | Value |
   | --- | --- | --- |
   | Advanced → AMD CBS → CPU Common Options | SMEE | Enable |
   | Advanced → AMD CBS → CPU Common Options | SEV Control | Enable |
   | Advanced → AMD CBS → CPU Common Options | SEV-ES ASID Space Limit | 99 |
   | Advanced → AMD CBS → CPU Common Options | SNP Memory (RMP Table) Coverage | Enabled |
   | Advanced → NBIO Common Options → IOMMU/Security | SEV-SNP Support | Enable |

   The ASID limit of 99 gives SEV-ES and SEV-SNP guests ASIDs 1 to 98. Three guests and a benchmark
   guest need four. The IOMMU must also be enabled, which it usually is by default, and the host
   check reads the kernel's verdict on it.

4. Save, and let the host boot.

**UNVERIFIED: whether the Advance-5's BIOS shows these options.** If an option is missing or
locked, open an OVHcloud support ticket. Ask them to enable SEV-SNP with the values above, and to
update the BIOS to the newest version that the board's vendor ships. Name the SEV firmware version
that you need, from AMD's latest SEV security bulletin for EPYC 8004 processors.

### Check SEV-SNP on the host

1. Read what the kernel found:

   ```shell
   sudo dmesg | grep -E 'SEV-SNP|RMP|SNP:'
   cat /sys/module/kvm_amd/parameters/sev_snp
   ```

   The output includes a line `ccp ...: SEV-SNP API:<major>.<minor> build:<build>`, which is the SEV
   firmware's version, and `kvm_amd: SEV-SNP enabled (ASIDs 1 - 98)`. The second command prints `Y`.
   A line ending `SNP cannot be supported` names what is missing: the IOMMU, its SNP feature or the
   RMP table.

2. Record the firmware version, and compare it with the minimum in AMD's latest SEV security
   bulletin for EPYC 8004 processors. **UNVERIFIED: the version this server ships.** A version
   below the bulletin's minimum needs a BIOS update from OVHcloud, because the report's TCB would
   fall below a release's minimum.

### Build QEMU

1. Build QEMU's latest 11.2 tag, which has the `secure-tsc` property, and install it under
   `/opt/qemu`. Use `v11.2.0` once it is tagged, and `v11.2.0-rc1` until then.

   **Changes the host.**

   ```shell
   git clone --branch v11.2.0-rc1 --depth 1 https://gitlab.com/qemu-project/qemu.git ~/qemu
   cd ~/qemu
   ./configure --prefix=/opt/qemu --target-list=x86_64-softmmu --enable-kvm --enable-slirp \
     --disable-docs
   make -j"$(nproc)"
   sudo make install
   cd ~/engine
   ```

2. Check that it offers the property:

   ```shell
   /opt/qemu/bin/qemu-system-x86_64 -object sev-snp-guest,help | grep secure-tsc
   ```

   The command prints a line with `secure-tsc=<bool>`.

### Run the host check

`scripts/snp-host-check.sh` reads, without changing anything, everything that a launch with Secure
TSC needs. It checks the processor and its generation, the CPUID bits for SEV-SNP and Secure TSC,
the C-bit position that the launch flags name, the ASIDs left for guests, the kernel and its SEV-SNP
initialisation, the SEV features that KVM accepts, the TSC, QEMU and the host's memory.

**Changes the host:** `modprobe cpuid` loads the kernel module through which the check reads
CPUID. The check itself only reads.

```shell
sudo modprobe cpuid
sudo QEMU=/opt/qemu/bin/qemu-system-x86_64 scripts/snp-host-check.sh
```

Every line starts with `ok`, the output ends with `PASSED`, and the line before it reads
`tee.processor = "Genoa"; verify attestations with --processor Genoa`. A `FAIL` line names the
missing piece and what supplies it. **UNVERIFIED: the check's verdict on this server.** If the
processor reports no Secure TSC bit, no guest can launch with the image's measurement, and the
server cannot be used for this work.

The `snphost ok` command, from the `virtee/snphost` project that AMD's SEV guide points to, checks
the same platform settings, and can confirm a failure that this check reports.

## Build the image

Build both variants from the same commit, as [the image's README](../../image/README.md#build)
describes. Each build takes tens of minutes on this host.

**Changes the host.**

```shell
scripts/build-image.sh --variant dev $SNP/image-dev
scripts/build-image.sh $SNP/image-production
cat $SNP/image-dev/measurement.json
```

The last command prints the `dev` variant's launch digest and its guest shape: 4 vCPUs of type
`EPYC-v4`, guest features `0x201` and policy `0x30000`. Compare
`$SNP/image-production/SHA256SUMS` with the `image-first` artifact of the CI run on the same
commit. The two must match, because the image is reproducible.

## Boot the three guests

The three guests, `a`, `b` and `c`, each run the `dev` variant, with their own vault on one local
chain. The `dev` variant accepts the local chain's settings and simulated prices, and it uses the
same real attestation, key sealing and launch flags as the production variant.

### Start the local chain

1. Point a DNS name that you control at the host's public IPv4 address, for example
   `chain.example.com`. Keep the name and the address in `~/snp/env`:

   **Changes the deployment's DNS.** The owner approves this step.

   ```shell
   echo "CHAIN_HOST=chain.example.com" >> ~/snp/env
   echo "HOST_IP=$(hostname -I | awk '{ print $1 }')" >> ~/snp/env
   . ~/snp/env
   getent hosts $CHAIN_HOST
   ```

   The last command prints the address in `$HOST_IP`, then the name.

2. Start Anvil on the host's loopback, mining a block a second, in its own `tmux` session:

   **Changes the host.**

   ```shell
   tmux new-session -d -s anvil \
     'anvil --host 127.0.0.1 --port 8545 --slots-in-an-epoch 1 --block-time 1'
   cast block-number --rpc-url http://127.0.0.1:8545
   ```

   The block number rises by one each second. `--slots-in-an-epoch 1` makes Anvil mark a block
   `safe` and `finalized` soon after it is mined, which the trading server waits for.

3. Put the chain behind TLS on four ports, for the three custody providers and the checkpoint
   endpoint, open only to the host itself. Caddy obtains a certificate for `$CHAIN_HOST` from Let's
   Encrypt, which needs the host's ports 80 and 443 free and reachable from the internet.

   **Changes the host.** The owner approves this step, which also asks a public authority for a
   certificate.

   ```shell
   sudo tee /etc/caddy/Caddyfile > /dev/null << EOF
   $CHAIN_HOST:18545, $CHAIN_HOST:18546, $CHAIN_HOST:18547, $CHAIN_HOST:18548 {
   	@outside not remote_ip $HOST_IP 127.0.0.1
   	abort @outside
   	reverse_proxy 127.0.0.1:8545
   }
   EOF
   sudo systemctl reload caddy
   curl -s https://$CHAIN_HOST:18545 -H 'content-type: application/json' \
     -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}'
   ```

   The last command prints `{"jsonrpc":"2.0","id":1,"result":"0x7a69"}`, which is chain 31337. If
   Caddy has no certificate, `journalctl -u caddy` says why. A guest reaches the host's public
   address through QEMU's user networking, so the proxy sees the guest's requests come from
   `$HOST_IP`. **UNVERIFIED: that source address.** If a guest's requests are refused, read the
   address from Caddy's log with `journalctl -u caddy` and add it to the matcher.

   **Warning: Anvil signs any transaction for its published accounts.** Without the `remote_ip`
   matcher, anyone on the internet could send transactions on the chain. Keep the matcher.

### Prepare each guest

`scripts/snp-local-chain.sh prepare` deploys a mock USDC, a timelock with a one-second delay, and a
vault whose signer is a placeholder. It writes the guest's `node.toml` from `config/anvil.toml`,
with real attestation, paths under `/data` and the chain at `$CHAIN_HOST`. It has the timelock name
the release, which is the image's launch digest and the SHA-256 of `node.toml`. It also writes
`release.toml`, a fresh operator key, the chain's addresses in `chain.env`, and the data disk's two
files under `disk/`.

1. Prepare the three guests, each with its own checkpoint relayer:

   **Changes the host:** the step deploys contracts on the local chain and sends timelock calls.

   ```shell
   for spec in a:3 b:4 c:5; do
     RELAYER_INDEX=${spec#*:} scripts/snp-local-chain.sh prepare \
       $SNP/${spec%:*} $SNP/image-dev $CHAIN_HOST
   done
   ```

   Each run ends with `prepared ...: vault 0x..., release named`. The relayers are Anvil's
   accounts 3, 4 and 5.

2. Make each guest's data disk and its log disk, which are the guest's first and second virtio disks and which belong on the host's two NVMe drives, one each. The file `new-key` on the disk makes the first boot seal new keys:

   ```shell
   for guest in a b c; do
     mke2fs -q -t ext4 -d $SNP/$guest/disk $SNP/$guest/data.img 64G
     mke2fs -q -t ext4 $SNP/$guest/log.img 64G
   done
   ```

### Launch the guests

`scripts/snp-launch.sh` builds the QEMU command from the image's `measurement.json`, with the
flags in [Boot on an SEV-SNP host](../../image/README.md#boot-on-an-sev-snp-host), and checks the
image's files against its `SHA256SUMS` first. Run `scripts/snp-launch.sh --dry-run ...` to print the
command without running it.

1. Launch each guest in its own `tmux` session, with its API and metrics forwarded to the host's
   loopback:

   **Changes the host.** The owner approves the launch of these guests.

   ```shell
   for spec in a:8081:9465 b:8082:9466 c:8083:9467; do
     IFS=: read -r guest api metrics <<< "$spec"
     sudo tmux new-session -d -s guest-$guest -c "$PWD" \
       "QEMU=/opt/qemu/bin/qemu-system-x86_64 scripts/snp-launch.sh --memory 16G \
        --forward $api:8080 --forward $metrics:9464 \
        $SNP/image-dev $SNP/$guest/data.img $SNP/$guest/log.img 2>&1 | tee $SNP/$guest/console.log"
   done
   ```

   Guest `a` serves its API on `127.0.0.1:8081` and its metrics on `127.0.0.1:9465`, guest `b` on
   8082 and 9466, and guest `c` on 8083 and 9467.

   `--forward` uses QEMU's user networking, which shows the guest every peer under one address. That
   is enough for these guests, which take no turns. A guest that serves members or clients needs
   networking that keeps each peer's own address, such as a bridged or TAP device: its handover and
   delivery listeners, and its API, count connections per address, so behind user networking one
   peer can hold every place.

2. Check that each guest launched with SEV-SNP and Secure TSC:

   ```shell
   grep -h 'SEV: Status' $SNP/*/console.log
   ```

   Each guest prints `SEV: Status: SEV SEV-ES SEV-SNP` followed by other features, including
   `SecureTSC`. A guest whose kernel does not time on a protected TSC stops before its trading
   server opens its keys, with the Critical alert `clocksource_unprotected`. A guest launched
   without Secure TSC also carries another launch digest, which the next procedure's step 2 shows.
   If QEMU exits at launch instead, its last lines in `console.log` name the cause:
   `VMSA contains unsupported sev_features` means that KVM does not offer Secure TSC, and a C-bit
   error means that `cbitpos` does not match the processor.

3. Wait for each guest to serve its attestation. Until the vault names its key, the trading server
   serves only `/v1/attestation`:

   ```shell
   for api in 8081 8082 8083; do curl -sk https://127.0.0.1:$api/v1/attestation | jq -r .enclave_address; done
   ```

   The command prints three enclave addresses.

### Verify each guest's attestation

Verify from outside the guests, with an `engine` built on the host. The verifier checks the report's
signature up to AMD's root, the launch digest and policy, the reported firmware versions, and the
report data's binding of the enclave address, the configuration and the TLS key.

1. Build the verifier:

   ```shell
   cargo build --release --locked -p engine-node --bin engine
   ```

2. Save each attestation, and read the fields that the verifier checks:

   ```shell
   for spec in a:8081 b:8082 c:8083; do
     guest=${spec%:*} report=$SNP/${spec%:*}/report.bin
     curl -sk https://127.0.0.1:${spec#*:}/v1/attestation > $SNP/$guest/attestation.json
     jq -r .quote $SNP/$guest/attestation.json | cut -c3- | xxd -r -p > $report
     echo "$guest version $(xxd -p -l 4 $report) policy $(xxd -p -s 0x08 -l 8 $report) key $(xxd -p -s 0x48 -l 1 $report)"
     echo "$guest reported TCB $(xxd -p -s 0x180 -l 8 $report) CPUID $(xxd -p -s 0x188 -l 3 $report)"
     echo "$guest digest $(xxd -p -s 0x90 -l 48 $report | tr -d '\n')"
     curl -sk https://127.0.0.1:${spec#*:}/v1/attestation/collateral | jq -r .vcek | cut -c3- | xxd -r -p \
       | openssl x509 -inform der -noout -text | grep -A1 '1.3.6.1.4.1.3704.1.2'
   done
   jq -r .launch_digest $SNP/image-dev/measurement.json
   ```

   For each guest, the version is one of `02000000` to `06000000`, the policy is
   `0000030000000000`, the key byte is `00`, which names the VCEK as the report's signing key, and
   the digest equals the last line, which is `measurement.json`'s `launch_digest`. Record the reported TCB. Its bytes are, in order, the boot loader, the TEE, four
   reserved bytes, the SNP firmware and the microcode security versions. Record the CPUID bytes,
   which are the processor's family, model and stepping: `19a002` on a Siena host. Record the VCEK's
   product name, which follows the extension `1.3.6.1.4.1.3704.1.2` after two characters of
   encoding, such as `..Genoa-A2`.

   **UNVERIFIED: the report version, the CPUID, the product name, the key and the digest.** The
   verifier refuses a version below `03000000` or above `06000000`, a family byte other than `19`,
   and a key byte that names a VLEK, the chip-secret VCEK or no key, and needs a change before step
   3 can pass. A CPUID other than `19a002` is not a Siena part, and the release does not accept it.
   A product name other than `Genoa-A2` means the guests' `release.toml` and `node.toml` must name
   it: prepare the guests again with `PRODUCT_NAME` set to it. A digest that differs from
   `measurement.json` means that `sev-snp-measure` and this QEMU disagree about the launch. Record
   the values and stop.

3. Run the verifier for each guest:

   ```shell
   for guest in a b c; do
     target/release/engine verify-attestation --attestation $SNP/$guest/attestation.json \
       --release $SNP/$guest/release.toml --processor Genoa
   done
   ```

   Each run prints `verified: release <guest>, enclave 0x..., config node.toml`.

4. Set the Siena entry's `minimum_tcb` in each `release.toml` to the floor that AMD's bulletins give
   for EPYC 8004 processors, and run step 3 again. Take each component's highest level across the
   bulletins' EPYC 8004 rows whose fix has shipped: TCB[SNP] 0x1D from AMD-SB-3038 and
   TCB[MICROCODE] 0x1B from AMD-SB-3027 as of 2026-11-22, with the boot loader and TEE at 0, since
   no bulletin sets them. Never take a number from another model's row: the same microcode level
   on a Genoa or Milan part stands for other fixes. A reported TCB below the floor fails the check,
   and needs the BIOS update that [Check SEV-SNP on the host](#check-sev-snp-on-the-host) describes.
   The production configuration's `[[tee.cpu_models]]` carries the same entry.

5. Check the collateral that each guest relays, which a browser verifier uses:

   ```shell
   curl -sk https://127.0.0.1:8081/v1/attestation/collateral > $SNP/a/collateral.json
   target/release/engine verify-attestation --attestation $SNP/a/attestation.json \
     --release $SNP/a/release.toml --processor Genoa --collateral $SNP/a/collateral.json
   ```

   The command prints the same `verified:` line as step 3.

### Register each guest's key and confirm it runs

1. Have each guest's timelock install the enclave address that its verified attestation names:

   **Changes the host:** the step sends timelock calls on the local chain.

   ```shell
   for guest in a b c; do
     enclave=$(jq -r .enclave_address $SNP/$guest/attestation.json)
     scripts/snp-local-chain.sh register $SNP/$guest $enclave
   done
   ```

   Each run ends with `the vault's signer is 0x...`, the guest's enclave address.

2. Wait about a minute, then check that each trading server serves its whole API and posts
   checkpoints:

   ```shell
   for spec in a:8081 b:8082 c:8083; do
     guest=${spec%:*}
     vault=$(awk -F= '$1 == "VAULT" { print $2 }' $SNP/$guest/chain.env)
     echo "$guest health $(curl -sk -o /dev/null -w '%{http_code}' https://127.0.0.1:${spec#*:}/v1/health)" \
       "latestSeq $(cast call $vault 'latestSeq()(uint64)' --rpc-url http://127.0.0.1:8545)"
   done
   ```

   Each guest prints `health 200`, and its `latestSeq` rises above zero once its first checkpoint is
   accepted. The vault accepts a checkpoint only under the signature of the key it names, so a
   rising `latestSeq` shows that the enclave's sealed key signs. Each guest raises `secret_missing`
   for `alerts_critical_url`, because no Critical alert route is delivered to it, and runs without
   that route.

3. Restart one guest, to show that its sealed keys open again. Stop QEMU with `Ctrl-A x` in the
   guest's `tmux` session (`sudo tmux attach -t guest-a`), then launch it again with step 1 of
   [Launch the guests](#launch-the-guests) for that guest alone. The disk no longer holds `new-key`,
   because the first boot removed it. The guest serves its whole API again, under the same enclave
   address.

Record in the release notes the vCPU model and count that the guests launched with, `EPYC-v4` and 4,
and the host's processor and SEV firmware version.

## Boot the production variant

The production variant refuses simulated prices and needs a certificate from an ACME authority for
its API, so it cannot run on this local setup. Boot it with `config/dev.toml`, as
`scripts/check-image.sh` does under plain QEMU, to show that it launches under SEV-SNP and refuses
the simulated providers:

**Changes the host.**

```shell
mkdir $SNP/production && grep -v '^allowed_origins' config/dev.toml > $SNP/production/config.toml
mke2fs -q -t ext4 -d $SNP/production $SNP/production.img 1G
mke2fs -q -t ext4 $SNP/production-log.img 1G
sudo QEMU=/opt/qemu/bin/qemu-system-x86_64 scripts/snp-launch.sh \
  $SNP/image-production $SNP/production.img $SNP/production-log.img | tee $SNP/production-console.log
grep -E 'SEV: Status|a production build refuses mock providers' $SNP/production-console.log
```

The guest prints its `SEV: Status` line with `SecureTSC`, refuses the simulated providers, and
powers off, which ends QEMU. The production variant's own launch digest is checked when its first
production configuration runs, as [First deployment](deploy-and-rollback.md#first-deployment)
describes. Both variants are measured by the same tool for the same guest shape, so a `dev` digest
that matches its report shows that the tool and this host agree.

## Measure inside the guest

These measurements set the production guest's vCPU count, memory and disk. Each benchmark runs
twice: inside an SEV-SNP guest that boots the image's own firmware and kernel with Secure TSC, and
on the host outside it, pinned to the same number of CPUs. Both runs use the same static binaries,
built with the release profile and with the musl C library and allocator of the image's own engine,
so the difference between the two is the guest's.

The benchmark guest boots an initramfs of its own, which holds the test binaries, busybox, and an
init that runs a list of benchmarks and powers off. Its launch digest is not the image's, which does
not matter for a measurement. Run it only while the three guests are stopped, or on a quiet host,
because the figures are latency measurements.

1. Write the list of benchmarks, one a line: a name, then the command. The commands are the
   repository's benchmarks, whose budgets the launch bar sets:

   ```shell
   cat > $SNP/bench.list << 'EOF'
   # Single commands on the sequencer's thread: a price, the rollover and signed batches.
   hot_path      /bench/hot_path_benchmark --ignored --nocapture --test-threads=1
   launch_book   /bench/launch_book_benchmark --ignored --nocapture --test-threads=1 launch_book_latency
   mass_events   /bench/mass_events_benchmark --ignored --nocapture --test-threads=1 mass_
   # A price's wait behind reads, checkpoints and the log's sync, and the stats scans.
   price_wait    /bench/engine_node --ignored --nocapture --test-threads=1 sequencer::wait_bench
   stats_scans   /bench/engine_node --ignored --nocapture --test-threads=1 stats::launch_bench
   # The trading server under launch flow and ten times it, and each way it recovers.
   load          /bench/engine_node --ignored --exact --nocapture load_scenario::dev_scenario_load
   recovery      RECOVERY_LAUNCH_SECS=3600 /bench/engine_node --ignored --exact --nocapture sequencer::recovery_scale::dev_scenario_recovery_scale
   EOF
   ```

2. Build the binaries and the benchmark guest's files from a checkout with no uncommitted changes:

   ```shell
   scripts/snp-bench-guest.sh build $SNP/bench $SNP/bench.list
   mke2fs -q -t ext4 -d $SNP/bench/disk $SNP/bench/disk.img 300G
   ```

   The first command ends with `built .../bench-initramfs.cpio.gz`. **UNVERIFIED: the static musl
   build of the test binaries,** which no machine has run yet. If a dependency does not build for
   musl, record its error.

3. Run the list inside an SEV-SNP guest of the production shape, 4 vCPUs, with 32 GiB of memory for
   the load test's clients and stand-ins:

   **Changes the host.**

   ```shell
   sudo QEMU=/opt/qemu/bin/qemu-system-x86_64 scripts/snp-launch.sh --memory 32G \
     --initrd $SNP/bench/bench-initramfs.cpio.gz $SNP/image-dev $SNP/bench/disk.img \
     | tee $SNP/bench/guest-console.log
   mkdir -p $SNP/bench/guest && debugfs -R "rdump /results $SNP/bench/guest" $SNP/bench/disk.img
   cat $SNP/bench/guest/results/guest.txt $SNP/bench/guest/results/status.txt
   ```

   `guest.txt` reads `clocksource: tsc`, the vCPU count and memory, and the `SEV: Status` line with
   `SecureTSC`. `status.txt` gives each benchmark's exit status and time. Each benchmark's output is
   in `results/<name>.txt`.

4. Run the same list on the host, pinned to four CPUs that are separate cores. The command below
   pins it to CPUs 0 to 3. Check with `lscpu -e` that those four CPUs show four different `CORE`
   values, and name four that do if they do not:

   ```shell
   scripts/snp-bench-guest.sh host $SNP/bench 0-3
   cat $SNP/bench/host-results/status.txt
   ```

5. Put the guest's and the host's figures side by side, for each budget the benchmarks print, in the
   evidence on the engine's performance at launch scale. Include these:
   - the price, rollover, batch-command and mass-event latencies, against their budgets;
   - a price's wait, at p50 and p99, and the batch sync time, which is the data disk's sync time;
   - the stats scans and the worker's lag;
   - the load test's order-to-fill, price-to-stamp and stream-lag percentiles, its busy share, and
     its CPU and resident memory;
   - the recovery test's replay rate, its restart from a cache, and its replica build.

6. Set the guest's shape from the figures. Its memory is at least four times the load test's
   resident memory. Its vCPU count covers the threads that the load keeps busy. Its data disk covers
   90 days of the log's measured growth. A vCPU count other than 4 changes the image's measurement,
   so it is a new image and a new release.

The load test drives a trading server that runs in the test's own process, so this procedure runs it
whole inside the guest, with its clients and price stand-ins beside it. Driving the image's own
trading server from outside the guest needs the load test to target a trading server at an address,
which it does not do yet.

The eight-hour soak at ten times launch flow runs the same way, in a guest of its own. Give it a
list with this one line, and make its data disk 600G, since its caches and history index can grow
by about 400 GB in a run:

```text
soak SOAK_DIR=/data/tmp/soak /bench/engine_node --ignored --exact --nocapture load_scenario::soak::dev_scenario_soak
```

### The log's own disk under the soak

A flush of the history index's and the recovery caches' buffered writes on the log's disk can hold the log's
batch syncs, which is why the image mounts the log on a disk of its own. Measure the difference on this host
with the soak at ten times launch flow, once for each leg, on the host as step 4 above runs a list (the guest's
launch script attaches a second disk only to the image's own init, which the benchmark guest does not use), one
leg after the other, with `<nvme0>` and `<nvme1>` the mount points of the host's two NVMe drives:

1. **Shared.** The log on the derived stores' drive. The list's line is
   `soak SOAK_DIR=<nvme0>/soak /bench/engine_node --ignored --exact --nocapture load_scenario::soak::dev_scenario_soak`,
   so the log is in `<nvme0>/soak/log` beside the caches and the history index.
2. **Separate.** The log on its own drive. The line also names `SOAK_LOG_DIR=<nvme1>/soak`, so the log is in
   `<nvme1>/soak/log` and the caches and history index stay in `<nvme0>/soak`. The two paths must be on
   different drives, as `df` shows, or the leg shows nothing.

Run both with the same `SOAK_HOURS`, `LOAD_ACCOUNTS` (10,000) and `SOAK_MULTIPLE` (10). Keep both reports. The
soak prints these lines for each interval, and the figures to record come from them:

- the `LOAD leg=… order_to_fill_us` line, which holds the interval's `p50`, `p99`, `p999` and `max`;
- each `LOAD leg=… window=…` line, which holds that window's `batch_p99_us`, `batch_p999_us`, `busy_share`,
  `host_io_wait_ms`, `host_cpu_wait_ms`, `host_disk_io` and `probe_blocked_ms`, and its `breaches` against #904's
  bars (a window prints no order-to-fill figure unless it breaches one).

Put the two legs side by side: each interval's `order_to_fill_us`, and each window's `batch_p99_us`,
`batch_p999_us` and `host_io_wait_ms`, with the number of windows that breached and the worst window of each leg.
State which leg has the lower order-to-fill tail and the lower batch tail, and by how much. Setting a bar on the
difference waits for these figures.

## How to tell it is done, and when to escalate

The host is ready, and the image is proven on it, when each of these holds:

- `scripts/snp-host-check.sh` passes.
- Each guest's console shows `SecureTSC` in its `SEV: Status` line.
- Each guest's report carries `measurement.json`'s launch digest and policy `0x30000`, and its
  report data starts with the SHA-256 of the bundle that `/v1/attestation` serves.
- `engine verify-attestation` verifies each guest against its `release.toml`, whose Siena entry
  carries the host's product name and AMD's floor for EPYC 8004, both from AMD's service and from
  the guest's relayed collateral.
- Each guest, once registered, serves its whole API and has a checkpoint accepted by its vault, and
  a restarted guest opens its sealed keys.
- The production variant launches and refuses the simulated providers.
- The benchmark figures are recorded for the guest and the host.

Escalate to the owner, with the step and its output, when any of these holds:

- the processor reports no Secure TSC, or the BIOS cannot enable SEV-SNP and OVHcloud will not;
- the SEV firmware is below AMD's minimum, and OVHcloud has no BIOS update that raises it;
- a report's version is below 3 or above 6, a report's CPUID is not Siena's `19a002`, a report
  names a signing key other than the VCEK, or a report's digest differs from `measurement.json`;
- a guest stops with `clocksource_unprotected`.

## Sources

Each source was read on 2026-11-22.

- OVHcloud, the confidential computing use case, listing the Advance-5 with the AMD EPYC 8224P,
  its memory and storage ranges:
  https://www.ovhcloud.com/en/bare-metal/uc-confidential-computing/
- OVHcloud Labs, the confidential computing attestation service (alpha, released 2026-11-18), with
  its SEV-SNP hardware compatibility as of 2026-10-22, its host systems and its firmware minimum:
  https://labs.ovhcloud.com/en/confidential-computing-occam/
- OVHcloud, the Bare Metal 2026 announcement (2026-02-09), on the Scale 2026 range's EPYC 9005
  processors: https://corporate.ovhcloud.com/en/newsroom/news/ovhcloud-bare-metal-2026-amd/
- OVHcloud, using the IPMI console with a dedicated server (updated 2026-01-29), on the KVM and
  serial consoles and on reaching the BIOS:
  https://docs.ovhcloud.com/en/guides/bare-metal-cloud/dedicated-servers/ipmi
- AMD, the AMDSEV repository's README, on the BIOS settings, the minimum host software and SEV
  firmware updates on EPYC 9004-class servers: https://github.com/AMDESE/AMDSEV
- AMD, AMDSEV issue 246, the CPUID leaf of an EPYC 8024P reporting Secure TSC, and the BIOS update
  that enabled SEV-SNP on it: https://github.com/AMDESE/AMDSEV/issues/246
- CVE-2023-31347 (published 2024-02-13), a Secure TSC flaw in the SEV firmware of third- and
  fourth-generation EPYC processors: https://www.cve.org/CVERecord?id=CVE-2023-31347
- Linux v6.18, KVM's SEV support, which offers Secure TSC to SEV-SNP guests (absent in v6.17):
  https://github.com/torvalds/linux/blob/v6.18/arch/x86/kvm/svm/sev.c
- Linux v6.18, the CPU feature list, where Secure TSC is CPUID Fn8000_001F_EAX bit 8:
  https://github.com/torvalds/linux/blob/v6.18/arch/x86/include/asm/cpufeatures.h
- Linux v6.18, the KVM device attribute that reports the SEV features KVM accepts:
  https://github.com/torvalds/linux/blob/v6.18/Documentation/virt/kvm/x86/amd-memory-encryption.rst
- Linux v6.18, the AMD IOMMU's refusal of SEV-SNP in passthrough mode:
  https://github.com/torvalds/linux/blob/v6.18/drivers/iommu/amd/init.c
- QEMU, the commit that adds `secure-tsc` (committed 2026-08-17), in `v11.2.0-rc0` (2026-11-10)
  and `v11.2.0-rc1` (2026-11-18) and not in `v11.1.3`:
  https://gitlab.com/qemu-project/qemu/-/commit/a26f9b975d5c0690ac5828fadb6242bfee79c008
- Ubuntu 26.04 packages, `linux-image-generic` 7.0.0-42.42 and `qemu-system-x86` 10.2.1:
  https://packages.ubuntu.com/resolute/linux-image-generic and
  https://packages.ubuntu.com/resolute/qemu-system-x86
- virtee `snpguest`, which fetches Siena's and Bergamo's certificates under the name `Genoa`:
  https://github.com/virtee/snpguest/blob/main/src/fetch.rs
- virtee `sev`, which maps CPUID family 19h models 10h to 1Fh and A0h to AFh to Genoa's certificate
  chain and models 00h to 0Fh to Milan's: https://github.com/virtee/sev/blob/main/src/lib.rs
- The Linux firmware repository's AMD microcode list, where family 19h model A0h has steppings 1
  and 2, whose patches are numbered `0x0aa001..` and `0x0aa002..`:
  https://git.kernel.org/pub/scm/linux/kernel/git/firmware/linux-firmware.git/plain/amd-ucode/README
- AMD-SB-3027 (StackWarp, CVE-2025-29943), whose EPYC 8004 row fixes Siena A2 with microcode
  `0x0AA0021B` and whose Bergamo row sets TCB[MICROCODE] 0x1B for the same microcode:
  https://www.amd.com/en/resources/product-security/bulletin/amd-sb-3027.html
- AMD-SB-3038 (published 2026-11-10), whose EPYC 8004 row sets TCB[SNP] 0x1D with GenoaPI 1.0.0.J
  of 2026-06-26: https://www.amd.com/en/resources/product-security/bulletin/AMD-SB-3038.html
- AMD-SB-3059 and AMD-SB-3061, whose EPYC 8004 fixes (TCB[SNP] 0x1E with GenoaPI 1.0.0.K, and Siena
  A2 microcode `0x0AA0021F`) are planned for December and November 2026:
  https://www.amd.com/en/resources/product-security/bulletin/amd-sb-3059.html and
  https://www.amd.com/en/resources/product-security/bulletin/amd-sb-3061.html
- AMD, the SEV Secure Nested Paging Firmware ABI Specification, publication 56860, revision 1.59
  (August 2026), whose Table 27 defines the attestation report of version 6 and the fields that
  versions 3 and 5 added: https://docs.amd.com/v/u/en-US/56860_PUB_SEV_SNP
- Google `go-sev-guest`, which reads the version-4 reports that SEV firmware 1.55.31 issues with
  version 3's layout:
  https://github.com/google/go-sev-guest/commit/b8d75ce2a7d4891d9e36e3dba4071f41ea7e73cd
- virtee `sev`, which reads attestation reports of versions 2, 3, 5 and 6:
  https://github.com/virtee/sev/blob/main/src/firmware/guest/types/snp.rs
