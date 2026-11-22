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
- **`engine verify-attestation` reads attestation reports of versions 2 and 3 only.** Newer SEV
  firmware can issue versions 5 and 6, which add fields after those the verifier reads. Whether
  this server's firmware issues a version the verifier reads is **UNVERIFIED**:
  [Verify each guest's attestation](#verify-each-guests-attestation) prints it. A later version
  needs a change to the verifier before the attestation can verify.
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
- [ ] **Storage:** at least 2 × 1.92 TB NVMe. The confidential computing page lists 2 × 960 GB to
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
| Disk for 90 days of launch flow | About 560 GB, before the history index | About 1.7 TB | A day of launch flow writes 5.7 GB of log. Daily snapshots add about 420 MB a day, and the two recovery caches up to 4 GiB in all. |
| Disk for one 8-hour soak at ten times launch flow | About 400 GB | | In the soak, at 10,000 accounts, the recovery caches and the history index grew about 12 KB for each command. |

The memory figure is not covered by the launch measurement, so a guest's memory can change without a
new release. The vCPU count is covered by it, so a different count is a new image. The 192 GB
covers three 16 GiB guests, a 32 GiB benchmark guest, the same benchmarks run on the host, and the
64 GB that a full 30-day replay at 100,000 accounts is projected to need. The memory of an
SEV-SNP guest is private to it and cannot be swapped, so the guests' memory must fit in the host's.

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

2. Make each guest's data disk. The file `new-key` on the disk makes the first boot seal new keys:

   ```shell
   for guest in a b c; do
     mke2fs -q -t ext4 -d $SNP/$guest/disk $SNP/$guest/data.img 64G
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
        $SNP/image-dev $SNP/$guest/data.img 2>&1 | tee $SNP/$guest/console.log"
   done
   ```

   Guest `a` serves its API on `127.0.0.1:8081` and its metrics on `127.0.0.1:9465`, guest `b` on
   8082 and 9466, and guest `c` on 8083 and 9467.

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
     echo "$guest version $(xxd -p -l 4 $report) policy $(xxd -p -s 0x08 -l 8 $report)"
     echo "$guest reported TCB $(xxd -p -s 0x180 -l 8 $report)"
     echo "$guest digest $(xxd -p -s 0x90 -l 48 $report | tr -d '\n')"
   done
   jq -r .launch_digest $SNP/image-dev/measurement.json
   ```

   For each guest, the version is `02000000` or `03000000`, the policy is `0000030000000000`, and
   the digest equals the last line, which is `measurement.json`'s `launch_digest`. Record the
   reported TCB. Its bytes are, in order, the boot loader, the TEE, four reserved bytes, the SNP
   firmware and the microcode security versions.

   **UNVERIFIED: the report version and the digest.** A version of `05000000` or `06000000` is
   refused by today's verifier, which needs a change before step 3 can pass. A digest that differs
   from `measurement.json` means that `sev-snp-measure` and this QEMU disagree about the launch.
   Record both values and stop.

3. Run the verifier for each guest:

   ```shell
   for guest in a b c; do
     target/release/engine verify-attestation --attestation $SNP/$guest/attestation.json \
       --release $SNP/$guest/release.toml --processor Genoa
   done
   ```

   Each run prints `verified: release <guest>, enclave 0x..., config node.toml`.

4. Set each `release.toml`'s `[minimum_tcb]` to the minimum that AMD's latest bulletin gives for
   EPYC 8004 processors, and run step 3 again. A reported TCB below that minimum fails the check,
   and needs the BIOS update that [Check SEV-SNP on the host](#check-sev-snp-on-the-host) describes.

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
sudo QEMU=/opt/qemu/bin/qemu-system-x86_64 scripts/snp-launch.sh \
  $SNP/image-production $SNP/production.img | tee $SNP/production-console.log
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

## How to tell it is done, and when to escalate

The host is ready, and the image is proven on it, when each of these holds:

- `scripts/snp-host-check.sh` passes.
- Each guest's console shows `SecureTSC` in its `SEV: Status` line.
- Each guest's report carries `measurement.json`'s launch digest and policy `0x30000`, and its
  report data starts with the SHA-256 of the bundle that `/v1/attestation` serves.
- `engine verify-attestation` verifies each guest against its `release.toml`, with a
  `[minimum_tcb]` from AMD's latest bulletin, both from AMD's service and from the guest's relayed
  collateral.
- Each guest, once registered, serves its whole API and has a checkpoint accepted by its vault, and
  a restarted guest opens its sealed keys.
- The production variant launches and refuses the simulated providers.
- The benchmark figures are recorded for the guest and the host.

Escalate to the owner, with the step and its output, when any of these holds:

- the processor reports no Secure TSC, or the BIOS cannot enable SEV-SNP and OVHcloud will not;
- the SEV firmware is below AMD's minimum, and OVHcloud has no BIOS update that raises it;
- a report's version is above 3, or its digest differs from `measurement.json`;
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
- virtee `sev`, which reads attestation reports of versions 2, 3, 5 and 6:
  https://github.com/virtee/sev/blob/main/src/firmware/guest/types/snp.rs
