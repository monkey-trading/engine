//! SEV-SNP sealing through the guest's `/dev/sev-guest`: the platform's report on this release, and
//! the key it derives for it. The key comes from the chip's VCEK and mixes in the launch
//! measurement, the guest policy, and a TCB version no higher than the committed one, so only this
//! release, launched under this policy on this chip, derives it, and a later firmware update still
//! derives the key sealed at the earlier TCB. The derivation runs in the platform's secure processor;
//! the key never passes through the host.

use anyhow::Context;
use engine_verify::SnpReport;
use sev::firmware::guest::{DerivedKey, Firmware, GuestFieldSelect};

use crate::tee::sealing::{Platform, Sealer};

/// The guest-request message version: version 1 needs no launch mitigation vector, which version 2
/// would mix into the key.
const MESSAGE_VERSION: u32 = 1;
/// The node runs at VMPL 0, with no paravisor below it.
const VMPL: u32 = 0;

/// The SEV-SNP platform this guest runs on.
pub(crate) struct SnpSealer;

impl Sealer for SnpSealer {
    /// The measurement, policy, and committed TCB from a report the platform issues now.
    /// Error cases:
    /// - the device cannot be opened or the platform refuses the report, or the report does not parse
    fn platform(&self) -> anyhow::Result<Platform> {
        let raw = Firmware::open()
            .context("opening /dev/sev-guest (is this an SEV-SNP guest?)")?
            .get_report(Some(MESSAGE_VERSION), Some([0; 64]), Some(VMPL))
            .map_err(|e| anyhow::anyhow!("the platform refused a report: {e}"))?;
        let report = SnpReport::parse(&raw)?;
        Ok(Platform { launch_digest: report.measurement, policy: report.policy, tcb: report.committed_tcb })
    }

    /// The VCEK-rooted key for this measurement and policy at `tcb`.
    /// Error cases:
    /// - the device cannot be opened, or the platform refuses the derivation (a `tcb` above the
    ///   committed one)
    fn sealing_key(&self, tcb: u64) -> anyhow::Result<[u8; 32]> {
        let mut fields = GuestFieldSelect::default();
        fields.set_measurement(true);
        fields.set_guest_policy(true);
        fields.set_tcb_version(true);
        let request = DerivedKey::new(false, fields, VMPL, 0, tcb, None);
        Firmware::open()
            .context("opening /dev/sev-guest (is this an SEV-SNP guest?)")?
            .get_derived_key(Some(MESSAGE_VERSION), request)
            .map_err(|e| anyhow::anyhow!("the platform refused the sealing key at TCB {tcb:#018x}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_mixes_in_the_measurement_the_policy_and_the_tcb_from_the_vcek() {
        let mut fields = GuestFieldSelect::default();
        fields.set_measurement(true);
        fields.set_guest_policy(true);
        fields.set_tcb_version(true);
        let request = DerivedKey::new(false, fields, VMPL, 0, 7, None);
        assert_eq!(request.get_root_key_select(), 0, "not the VCEK");
        let selected = request.guest_field_select;
        assert!(selected.get_measurement() && selected.get_guest_policy() && selected.get_tcb_version());
        // Neither the image nor the family ID, which an ID key the operator holds would set.
        assert!(!selected.get_image_id() && !selected.get_family_id() && !selected.get_svn());
        assert_eq!(request.tcb_version, 7);
    }
}
