//! The guest clock a node with real attestation must run on before it opens its keys.
//!
//! Every bound the node keeps in monotonic time rests on that clock's rate: a price's age, the pace
//! of sequenced time, a trusted time sample's age, the Roughtime round trips that place it, and
//! through them `time.max_drift`. The image keeps the rate out of the host's hands in three parts:
//!
//! - Secure TSC. Every vCPU launches with SEV_FEATURES `0x201` (SNPActive, SecureTSC). The guest's
//!   RDTSC, RDTSCP and TSC MSR read the processor's TSC scaled by the GUEST_TSC_SCALE in the guest's
//!   encrypted VMSA plus its GUEST_TSC_OFFSET, and the processor's TSC ratio and the hypervisor's
//!   offset do not apply (AMD64 APM vol. 2, 15.36.18). The firmware sets each launched vCPU's scale
//!   from the frequency the host asks for, and the kernel takes the values for the vCPUs it starts
//!   from the firmware's TSC_INFO answer (SEV-SNP firmware ABI, 7.9 and 8.18.2.2). The kernel reads
//!   the frequency from GUEST_TSC_FREQ, corrected by the TSC_FACTOR in the secrets page, and stops
//!   the guest if the host intercepts that read or RDTSC (Linux `arch/x86/coco/sev/`).
//! - The kernel times on the TSC: Secure TSC marks it reliable, `no-kvmclock` on the measured
//!   command line keeps out kvm-clock, and the image's kernel has no CPU frequency scaling or ACPI
//!   processor driver, so nothing the host supplies marks the TSC unstable once it has registered.
//! - This check. At boot the host can still have the kernel mark the TSC unstable, through CPUID
//!   leaves naming another hypervisor (the firmware checks only the standard and extended leaves) or
//!   firmware tables, and it picks the frequency at launch, so a node with `configfs_tsm` refuses to
//!   start with `clocksource_unprotected` unless the kernel times on the TSC at
//!   `MIN_GUEST_TSC_FREQ_MHZ` or more. The bound rests on the APM's statement that GUEST_TSC_FREQ
//!   reports the guest TSC's effective frequency, which only SEV-SNP hardware can confirm.
//!
//! The report carries no SEV features (ABI, Table 27), but the launch digest covers each vCPU's VMSA,
//! SEV_FEATURES included, measured as if its TSC scale and offset were zero, since the firmware sets
//! them afterwards (ABI, 8.18.2.2). A launch without Secure TSC measures to another digest.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

use anyhow::{Context, ensure};

/// Where the guest kernel names the clocksource its monotonic and wall clocks read.
const CURRENT_CLOCKSOURCE: &str = "/sys/devices/system/clocksource/clocksource0/current_clocksource";
/// GUEST_TSC_FREQ (AMD64 APM vol. 2, 15.36.18): the guest TSC's effective frequency in whole MHz.
const GUEST_TSC_FREQ_MSR: u64 = 0xC001_0134;
/// Bits 17:0 of GUEST_TSC_FREQ hold the frequency in MHz.
const GUEST_TSC_FREQ_MHZ_MASK: u64 = 0x3_FFFF;
/// The least frequency the check accepts. Under it the firmware rounds the host's requested
/// frequency below what the hardware reports, and the kernel calibrates against clocks the host
/// emulates instead (`docs/enclave.md`, "The guest's clock"). 1000 MHz also bounds the whole-MHz
/// rounding to 0.1%, the most a bound resting on the clock's rate can give up.
const MIN_GUEST_TSC_FREQ_MHZ: u64 = 1000;

/// The guest kernel's clock as the start check reads it: the clocksource it times on and the
/// frequency the hardware reports for it.
pub(crate) struct GuestClock {
    clocksource: PathBuf,
    msr: PathBuf,
}

impl Default for GuestClock {
    fn default() -> Self {
        Self { clocksource: PathBuf::from(CURRENT_CLOCKSOURCE), msr: PathBuf::from("/dev/cpu/0/msr") }
    }
}

impl GuestClock {
    /// Checks that the guest times on the TSC, in a frequency the hardware reports. Secure TSC
    /// makes the TSC the processor's, scaled by a factor the firmware sets, so the host cannot
    /// make the guest count in another rate or change it afterwards; a host that intercepts the
    /// frequency read terminates the guest (Linux `arch/x86/coco/sev/`).
    /// Error cases:
    /// - `clocksource` cannot be read, or names another clocksource than the TSC
    /// - `msr` cannot be read, or reports a guest TSC frequency under 1000 MHz
    pub(crate) fn require_protected(&self) -> anyhow::Result<()> {
        let current = std::fs::read_to_string(&self.clocksource)
            .with_context(|| format!("reading {}", self.clocksource.display()))?;
        ensure!(
            current.trim_end() == "tsc",
            "the guest kernel times on {:?}, not on the TSC that Secure TSC protects",
            current.trim_end()
        );
        let msr = File::open(&self.msr).with_context(|| format!("opening {}", self.msr.display()))?;
        let mut raw = [0u8; 8];
        msr.read_exact_at(&mut raw, GUEST_TSC_FREQ_MSR)
            .with_context(|| format!("reading MSR {GUEST_TSC_FREQ_MSR:#X} from {}", self.msr.display()))?;
        let mhz = u64::from_le_bytes(raw) & GUEST_TSC_FREQ_MHZ_MASK;
        ensure!(
            mhz >= MIN_GUEST_TSC_FREQ_MHZ,
            "the hardware reports a guest TSC frequency of {mhz} MHz, under {MIN_GUEST_TSC_FREQ_MHZ} MHz"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_tsc_in_a_hardware_frequency_passes() {
        let clock = clock("tsc-in-hardware-frequency", "tsc", 3_700);
        clock.require_protected().expect("the TSC at the hardware's frequency passes");
    }

    #[test]
    fn another_clocksource_refuses_before_the_msr_is_read() {
        // kvm-clock's scale is the host's to write; the others the host emulates; tsc-early is
        // the boot clock, gone once the TSC registers.
        for (i, other) in ["kvm-clock", "hpet", "acpi_pm", "jiffies", "tsc-early", ""].iter().enumerate() {
            let clock = clock(&format!("other-clocksource-{i}"), other, 3_700);
            let err = clock.require_protected().unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("the guest kernel times on {other:?}, not on the TSC that Secure TSC protects")
            );
        }
        let base = clocksource_only("absent-clocksource", "tsc");
        let missing_path = base.clocksource.with_file_name("absent");
        let absent = GuestClock { clocksource: missing_path, ..base };
        let err = absent.require_protected().unwrap_err();
        assert_eq!(err.to_string(), format!("reading {}", absent.clocksource.display()));
    }

    #[test]
    fn a_frequency_under_1000_mhz_refuses() {
        // Below 1000 MHz the whole-MHz reading can round the frequency by more than 0.1%, and at
        // 0 MHz the kernel calibrates against clocks the host emulates.
        for mhz in [0, 1, 999] {
            let clock = clock(&format!("frequency-{mhz}"), "tsc", mhz);
            let err = clock.require_protected().unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("the hardware reports a guest TSC frequency of {mhz} MHz, under 1000 MHz")
            );
        }
        // The floor's boundary: 1000 MHz passes, 999 refuses.
        clock("frequency-1000", "tsc", 1_000).require_protected().expect("1000 MHz passes");
    }

    #[test]
    fn a_missing_or_short_msr_refuses() {
        let dir = crate::test_support::scratch_dir("guest-clock-msr");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("current_clocksource"), "tsc\n").unwrap();
        let missing = GuestClock { clocksource: dir.join("current_clocksource"), msr: dir.join("msr") };
        let err = missing.require_protected().unwrap_err();
        assert_eq!(err.to_string(), format!("opening {}", missing.msr.display()));
        // A host-shaped MSR file shorter than the offset is a refused read, not a pass.
        std::fs::write(&missing.msr, [0u8; 8]).unwrap();
        let err = missing.require_protected().unwrap_err();
        assert_eq!(err.to_string(), format!("reading MSR {GUEST_TSC_FREQ_MSR:#X} from {}", missing.msr.display()));
    }

    // ── Helpers ──

    /// A clock over a scratch `current_clocksource` naming `clocksource` and a sparse `msr` file
    /// holding `mhz` at the GUEST_TSC_FREQ offset, like `/dev/cpu/0/msr`.
    fn clock(name: &str, clocksource: &str, mhz: u64) -> GuestClock {
        let c = clocksource_only(name, clocksource);
        let msr = c.msr.with_file_name("msr");
        let file = File::create(&msr).unwrap();
        file.write_all_at(&mhz.to_le_bytes(), GUEST_TSC_FREQ_MSR).unwrap();
        GuestClock { msr, ..c }
    }

    fn clocksource_only(name: &str, clocksource: &str) -> GuestClock {
        let dir = crate::test_support::scratch_dir(name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("current_clocksource");
        // The kernel writes the name and a newline.
        std::fs::write(&path, format!("{clocksource}\n")).unwrap();
        GuestClock { clocksource: path, msr: dir.join("msr") }
    }
}
