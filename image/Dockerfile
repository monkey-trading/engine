# check=skip=FromPlatformFlagConstDisallowed
# Every stage pins linux/amd64 on purpose: the image is x86_64 whatever machine builds it.
#
# The enclave image: the engine in a busybox initramfs, a Linux kernel for AMD SEV-SNP guests, the
# OVMF firmware that boots them, and the SEV-SNP launch measurement of all three. Every input is
# pinned by digest, checksum, or commit, and every timestamp comes from SOURCE_DATE_EPOCH, so a
# commit builds to the same bytes on any machine. Every download a build step makes retries a
# transient failure from the same source, with growing waits, for at least three minutes in all.
#
# Build it with scripts/build-image.sh, which feeds in the committed tree and sets
# SOURCE_DATE_EPOCH to the commit time. The output is vmlinuz, initramfs.cpio.gz, OVMF.fd,
# measurement.json, and SHA256SUMS.

ARG SOURCE_DATE_EPOCH
# `production` refuses mock TEE, custody, and feed providers; `dev` allows them.
ARG VARIANT=production
ARG DEBIAN_SNAPSHOT=20261005T000000Z
# The toolchain of the rust base image, which its digest below pins.
ARG RUST_VERSION=1.99.1
ARG RUST_STD_MUSL_SHA256=1dd241eaeb19ff3b5180461c77bdee93d6cafc669eda9de834e7c06b0ec3ca95
ARG KERNEL_VERSION=6.18.57
ARG KERNEL_SHA256=5bd9acdc82ec5efc3790ae840e5aaa40dfde7efff261cf6dde902dc570acf98c
# edk2-stable202608
ARG EDK2_COMMIT=2970e5699ba6267f3384ffab20f96647578aebc8
# The guest the launch measurement is computed for. The VM must launch with the same values.
ARG VCPUS=4
ARG VCPU_TYPE=EPYC-v4
ARG GUEST_FEATURES=0x1
ARG CMDLINE="console=ttyS0 ip=dhcp panic=-1"
# The SEV-SNP guest policy a verifier requires: SMT allowed, debug and migration agent off. The
# launch digest does not cover it, so the report must be checked for it separately.
ARG POLICY=0x30000

# Build tools for the kernel, firmware, and initramfs, from a fixed Debian snapshot. The snapshot
# archive redirects every download, which apt's HTTP pipelining mismatches, so pipelining is off.
# apt retries an index or package download that meets a network error or an HTTP 408, 429, or 5xx
# up to ten times, 1 s doubling to at most 30 s apart: about three minutes. An index still missing
# after that fails `apt-get update`, rather than letting apt install from the suites it did fetch.
FROM --platform=linux/amd64 debian:trixie-20261005-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f AS tools
ARG DEBIAN_SNAPSHOT
RUN printf '%s\n' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian/$DEBIAN_SNAPSHOT" \
      'Suites: trixie trixie-updates' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.pgp' \
      '' \
      'Types: deb' \
      "URIs: http://snapshot.debian.org/archive/debian-security/$DEBIAN_SNAPSHOT" \
      'Suites: trixie-security' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.pgp' \
      > /etc/apt/sources.list.d/debian.sources \
 && printf '%s\n' \
      'Acquire::Check-Valid-Until "false";' \
      'Acquire::Retries "10";' \
      'Acquire::http::Pipeline-Depth "0";' \
      'APT::Update::Error-Mode "any";' \
      > /etc/apt/apt.conf.d/80snapshot \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      acpica-tools bc bison build-essential busybox-static ca-certificates cpio curl flex git \
      libelf-dev libssl-dev nasm python3 uuid-dev xz-utils \
 && rm -rf /var/lib/apt/lists/*

# The musl target's standard library for the pinned toolchain, checked against its SHA-256.
FROM tools AS rust-std
ARG RUST_VERSION
ARG RUST_STD_MUSL_SHA256
WORKDIR /build
RUN curl -fsSL --retry 8 --retry-all-errors --connect-timeout 30 --speed-limit 1000 --speed-time 60 \
      -o rust-std.tar.xz "https://static.rust-lang.org/dist/rust-std-$RUST_VERSION-x86_64-unknown-linux-musl.tar.xz" \
 && echo "$RUST_STD_MUSL_SHA256  rust-std.tar.xz" | sha256sum -c - \
 && tar -xJf rust-std.tar.xz \
 && mv "rust-std-$RUST_VERSION-x86_64-unknown-linux-musl/rust-std-x86_64-unknown-linux-musl/lib/rustlib/x86_64-unknown-linux-musl" /rust-std

# The engine as one static x86_64 binary.
FROM --platform=linux/amd64 rust:${RUST_VERSION}-slim-trixie@sha256:4df2ae8cfdfebdfc13a65da7f253b73d68983dc0339f2719bc17a279c913b13d AS node
ARG VARIANT
COPY --from=rust-std /rust-std /tmp/rust-std
RUN mv /tmp/rust-std "$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-musl"
# ring compiles C for the target, so the musl C compiler comes from the same Debian snapshot.
COPY --from=tools /etc/apt/sources.list.d/debian.sources /etc/apt/sources.list.d/debian.sources
COPY --from=tools /etc/apt/apt.conf.d/80snapshot /etc/apt/apt.conf.d/80snapshot
RUN apt-get update \
 && apt-get install -y --no-install-recommends musl-tools \
 && rm -rf /var/lib/apt/lists/*
ENV CC_x86_64_unknown_linux_musl=musl-gcc
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Embedded in the binary: the published test keys a real enclave refuses as its operator key.
COPY config/test-keys.toml ./config/test-keys.toml
RUN case "$VARIANT" in \
      production) features="--features production" ;; \
      dev) features="" ;; \
      *) echo "VARIANT must be production or dev, not '$VARIANT'" >&2; exit 1 ;; \
    esac \
 && cargo build --locked --config net.retry=20 --release --target x86_64-unknown-linux-musl -p engine-node --bin engine $features \
 && cp target/x86_64-unknown-linux-musl/release/engine /engine

# Linux for an SEV-SNP guest: one bzImage, no loadable modules.
FROM tools AS kernel
ARG SOURCE_DATE_EPOCH
ARG KERNEL_VERSION
ARG KERNEL_SHA256
WORKDIR /build
RUN curl -fsSL --retry 8 --retry-all-errors --connect-timeout 30 --speed-limit 1000 --speed-time 60 \
      -o linux.tar.xz "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$KERNEL_VERSION.tar.xz" \
 && echo "$KERNEL_SHA256  linux.tar.xz" | sha256sum -c - \
 && tar -xJf linux.tar.xz \
 && mv "linux-$KERNEL_VERSION" linux \
 && rm linux.tar.xz
COPY image/kernel.config ./kernel.config
RUN : "${SOURCE_DATE_EPOCH:?SOURCE_DATE_EPOCH must be set}" \
 && cd linux \
 && make -s x86_64_defconfig \
 && scripts/kconfig/merge_config.sh -m .config ../kernel.config > /dev/null \
 && make -s olddefconfig \
 && grep -E '^(CONFIG_|# CONFIG_)' ../kernel.config | while read -r line; do \
      grep -qxF "$line" .config || { echo "kernel config did not keep: $line" >&2; exit 1; }; \
    done \
 && make -s -j"$(nproc)" bzImage \
      KBUILD_BUILD_TIMESTAMP="$(LC_ALL=C date -u -d "@$SOURCE_DATE_EPOCH")" \
      KBUILD_BUILD_USER=engine KBUILD_BUILD_HOST=engine KBUILD_BUILD_VERSION=1 \
 && cp arch/x86/boot/bzImage /vmlinuz

# OVMF for SEV-SNP direct boot (AmdSevX64). It checks the kernel, initramfs, and command line
# against the hashes QEMU places in the measured launch, so the launch measurement covers them.
FROM tools AS ovmf
ARG EDK2_COMMIT
# A fetch slower than 1 KB/s for a minute aborts and is retried, so a stalled mirror fails the
# build instead of hanging it.
ENV GIT_HTTP_LOW_SPEED_LIMIT=1000 GIT_HTTP_LOW_SPEED_TIME=60
WORKDIR /build/edk2
RUN git init -q \
 && delay=15 \
 && for attempt in 1 2 3 4 5; do \
      git fetch -q --depth 1 https://github.com/tianocore/edk2.git "$EDK2_COMMIT" \
      && git checkout -q FETCH_HEAD \
      && git submodule update -q --init --depth 1 --jobs 8 \
      && break; \
      [ "$attempt" -lt 5 ] || exit 1; \
      echo "edk2 fetch attempt $attempt failed, retrying in ${delay}s" >&2; \
      sleep "$delay"; \
      delay=$((delay * 2)); \
    done
# An empty grub.efi skips building GRUB: the image always boots its kernel directly.
RUN make -s -C BaseTools -j"$(nproc)" \
 && touch OvmfPkg/AmdSev/Grub/grub.efi \
 && bash -c '. ./edksetup.sh && build -q -a X64 -b RELEASE -t GCC -p OvmfPkg/AmdSev/AmdSevX64.dsc -n "$(nproc)"' \
 && cp Build/AmdSev/RELEASE_GCC/FV/OVMF.fd /OVMF.fd

# The initramfs: busybox for the init script, and the engine. The kernel's DHCP client writes the
# network's resolvers to /proc/net/pnp, which resolv.conf points at.
FROM tools AS initramfs
ARG SOURCE_DATE_EPOCH
WORKDIR /initramfs
COPY --from=node /engine ./engine
COPY image/init ./init
RUN : "${SOURCE_DATE_EPOCH:?SOURCE_DATE_EPOCH must be set}" \
 && mkdir bin data dev etc proc sys \
 && cp /bin/busybox bin/busybox \
 && for applet in sh mount sync poweroff; do ln -s busybox "bin/$applet"; done \
 && ln -s /proc/net/pnp etc/resolv.conf \
 && chmod 0755 . bin data dev etc proc sys bin/busybox engine init \
 && find . -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} + \
 && find . -print0 | LC_ALL=C sort -z \
    | cpio --quiet --null --create --format=newc --owner=0:0 --reproducible \
    | gzip -9 -n > /initramfs.cpio.gz

# The SEV-SNP launch measurement of the firmware, kernel, initramfs, and command line together.
FROM --platform=linux/amd64 python:3.14.8-slim-trixie@sha256:a2b82f3c48559aa0a8446d9af49826b6e2b2016f4cd2afabfe6013ec53729170 AS measure
COPY image/requirements.txt /measure/requirements.txt
RUN pip install --quiet --no-cache-dir --root-user-action=ignore --no-deps --only-binary=:all: \
      --retries 10 --require-hashes -r /measure/requirements.txt
WORKDIR /out
COPY --from=ovmf /OVMF.fd ./OVMF.fd
COPY --from=kernel /vmlinuz ./vmlinuz
COPY --from=initramfs /initramfs.cpio.gz ./initramfs.cpio.gz
ARG VARIANT
ARG VCPUS
ARG VCPU_TYPE
ARG GUEST_FEATURES
ARG CMDLINE
ARG POLICY
RUN digest="$(sev-snp-measure --mode snp --vmm-type QEMU --vcpus "$VCPUS" --vcpu-type "$VCPU_TYPE" \
      --guest-features "$GUEST_FEATURES" --ovmf OVMF.fd --kernel vmlinuz --initrd initramfs.cpio.gz \
      --append "$CMDLINE" --output-format hex)" \
 && python3 -c 'import json, sys; \
      keys = ["variant", "launch_digest", "vcpus", "vcpu_type", "guest_features", "cmdline", "policy"]; \
      fields = dict(zip(keys, sys.argv[1:])); \
      fields.update(vcpus=int(fields["vcpus"]), vmm_type="QEMU"); \
      print(json.dumps(fields, indent=2, sort_keys=True))' \
      "$VARIANT" "$digest" "$VCPUS" "$VCPU_TYPE" "$GUEST_FEATURES" "$CMDLINE" "$POLICY" > measurement.json \
 && sha256sum OVMF.fd initramfs.cpio.gz measurement.json vmlinuz > SHA256SUMS

FROM scratch
COPY --from=measure /out/ /
