# The enclave guest

This page describes the boundary between the trading server's confidential virtual machine and the
host that runs it: what the guest takes from the host, and how it keeps the host from altering the
rate of its clock. [The image's README](../image/README.md) describes how the image is built,
booted and released, and [the security model](security-model.md) states what each part of the
system is trusted for.

## What the guest takes from the host

The engine takes input from the host through the following paths. Configuration keys call a
running trading server a node, as the first one below does.

- **The API port**, `node.listen`, which serves TLS with a key generated inside the enclave. It also
  answers the ACME authority's TLS-ALPN-01 validations, so the host forwards port 443 to it.
- **The handover port**, `tee.sealing.handover.listen`, where the next release asks for the
  enclave's keys. The request and the answer are attested and the keys are encrypted, so the host
  only carries them. Each side takes the other's quote only from a CPU model that its configuration's
  `[[tee.cpu_models]]` lists, at that model's minimum firmware and microcode versions, so a host
  cannot offer a peer on a processor whose versions mean fewer fixes.
- **The delivery port**, `tee.sealing.secrets.listen`, where the operator delivers the server's
  secrets. The offer and the receipt are attested and the bundle is encrypted and signed.
- **The metrics port**, `metrics.listen`, which serves `GET /metrics` over plain HTTP.
- **The outbound network**: the venue feeds over TLS, the custody and checkpoint RPC endpoints over
  HTTPS, the ACME directory, alert webhooks, Roughtime servers, and DNS from the resolvers that DHCP
  names. The server also fetches its chip's certificates from AMD's key distribution service at
  `tee.kds_base`, so the deployment must allow outbound HTTPS to that address. It relays them only
  when the chip's certificate names the product line `tee.processor` names.
- **The wall clock**, which stamps every command and which signed Roughtime readings bound.
- **The data disk**, an ext4 filesystem on the first virtio disk, mounted at `/data` with `nosuid`,
  `nodev` and `noexec`. It holds `config.toml`, the history index, the recovery caches, the alert
  outbox, the custody cursor and the sealed keys. The log is on the second virtio disk, mounted at
  `/data/log` with the same options, so that the buffered writes of the first never hold the log's
  syncs, and a production build refuses a configuration whose log directory shares a device with
  the history index, the recovery caches or the daily snapshots. The root filesystem and `/dev` are read-only, so a path in the
  configuration outside `/data` fails at boot. The keys are sealed through `/dev/sev-guest` to this
  release's launch measurement and policy on this chip, so the host holds only ciphertext.

Two files on the data disk are the operator's one-off choices, and [init](../image/init) removes
both before the engine starts. `/data/new-key` seals new keys for a release that has none, and
`/data/predecessor` holds the `host:port` of the running release from which to take them.

The kernel also reads input that the launch measurement does not cover, and nothing yet bounds what
a crafted input can do to it: the ext4 filesystem that the host builds, the ACPI tables that QEMU
supplies, the emulated virtio devices, serial port and real-time clock, and the DHCP replies.
Authenticating the data and log disks and measuring the ACPI tables are not yet done.

The guest runs no login or other service. [init](../image/init) mounts the filesystems and runs
the engine, and when the engine exits it syncs the disk and powers the guest off. The guest writes
the engine's logs to the serial console and ignores an ACPI shutdown request, so every stop is a
hard stop. The
engine syncs each command to the disk before acknowledging it, so a hard stop loses nothing a
client was told. However, it does not write the final checkpoint that a drained shutdown writes,
so the commands after the last checkpoint wait for the next server to sign one.

## The guest's clock

Every bound that the server keeps in monotonic time depends on that clock's rate. The bounds include
a price's age, the pace of sequenced time, the age of a trusted time sample and, through those,
`time.max_drift`. The image keeps the clock's rate out of the host's control in three ways:

- **Secure TSC.** Every vCPU launches with the SEV features `0x201`, which are SNPActive (bit 0)
  and SecureTSC (bit 9). The guest's timestamp counter then reads the processor's counter scaled by
  values held in the guest's encrypted state, and the host's own scaling and offset do not apply.
  The host chooses the frequency at launch, but cannot make the guest count at another frequency or
  change it afterwards. The kernel stops the guest if the host intercepts a read of the counter or
  of its frequency.
- **The kernel times on the TSC.** With Secure TSC the kernel treats the TSC as reliable. The
  measured command line includes `no-kvmclock`, which excludes a clock whose scale the host writes.
  The kernel's [configuration](../image/kernel.config) has no CPU frequency scaling and no ACPI
  processor driver, so nothing the host supplies can mark the TSC unstable once it is in use.
- **The server checks.** At boot, the host could still make the kernel distrust the TSC, for example
  by naming another hypervisor in its CPUID leaves, and the kernel would then time on a clock that
  QEMU emulates. The host could also request a frequency too low for the hardware to count in.
  Before a server with real attestation opens its keys, it therefore reads the kernel's clocksource
  and the guest's TSC frequency. It refuses to start, raising the Critical alert
  `clocksource_unprotected`, unless the kernel times on the TSC at 1000 MHz or more.

The attestation report does not carry the SEV features, but the launch measurement covers each
vCPU's features. A launch without Secure TSC therefore measures to a different digest, and a
verifier requires the release's `guest_features` to include both bits. The `dev` variant launches
with the same features. Under plain QEMU, which has no SEV-SNP, the guest times on whatever clock
QEMU provides, and the simulated enclave skips the server's check.

The documentation of `crates/node/src/tee/clock.rs` cites the processor manual, the SEV-SNP firmware
interface and the Linux sources for each of these steps. **None of the three has yet been confirmed
on SEV-SNP hardware.**
