//! The CPU models a release accepts quotes from. The vendor numbers each TCB component per CPU model,
//! so the same level means different fixes on different models, and a release lists each model it
//! accepts with the lowest TCB it takes from that model. A quote names its model three ways, and all
//! three must agree with the entry it is held to: the report's CPUID family, model and stepping (from
//! report version 3), the VCEK's product name, and the product line of the vendor root and
//! intermediate that issued the VCEK. A quote's TCB is then compared with its own model's minimum only.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use anyhow::{Context, ensure};
use serde::Deserialize;

use crate::release::MinimumTcb;
use crate::report::{Cpuid, FAMILY_19H, Tcb};

/// The product lines the vendor certifies family 19h chips under, by the CPUID models each covers:
/// Milan's 00h to 0Fh, and Genoa's 10h to 1Fh and A0h to AFh, which take in Genoa-X, Bergamo and
/// Siena. A line names the vendor's root (`ARK-<line>`), its intermediate (`SEV-<line>`) and the start
/// of each VCEK's product name.
#[allow(clippy::single_range_in_vec_init)]
const PRODUCT_LINES: [(&str, &[RangeInclusive<u8>]); 2] =
    [("Milan", &[0x00..=0x0F]), ("Genoa", &[0x10..=0x1F, 0xA0..=0xAF])];

/// A CPU model whose quotes a release accepts, named as its reports and its VCEKs name it, with the
/// lowest TCB accepted from it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CpuModel {
    /// The product name the vendor's VCEKs for this model carry (extension 1.3.6.1.4.1.3704.1.2): the
    /// product line and the stepping, such as `Milan-B0`.
    pub product_name: String,
    /// The CPUID family, model and stepping a report of version 3 or later carries.
    pub family: u8,
    pub model: u8,
    pub stepping: u8,
    /// The lowest TCB a quote from this model is accepted at, from the vendor's bulletins for it.
    pub minimum_tcb: MinimumTcb,
}

/// What a quote's report and certificates say its chip is, once the report names a CPUID and the
/// VCEK's product line is its chain's and the CPUID's.
pub(crate) struct Chip {
    pub cpuid: Cpuid,
    pub product_name: String,
}

impl CpuModel {
    /// The CPUID the model's reports carry.
    pub fn cpuid(&self) -> Cpuid {
        Cpuid { family: self.family, model: self.model, stepping: self.stepping }
    }

    /// Checks one list of models, a release manifest's or a node's, which messages call `what`.
    /// Error cases:
    /// - the list is empty, or two entries name one CPUID
    /// - an entry's product name is not a product line, a hyphen and an alphanumeric stepping, or its
    ///   line is not the one the vendor certifies its CPUID family and model under
    pub fn validate_list(models: &[CpuModel], what: &str) -> anyhow::Result<()> {
        ensure!(!models.is_empty(), "{what} lists no CPU model, so it would accept no quote");
        let mut seen = BTreeSet::new();
        for model in models {
            let cpuid = model.cpuid();
            ensure!(seen.insert((cpuid.family, cpuid.model, cpuid.stepping)), "{what} lists {} twice", describe(cpuid));
            let stepping = model.product_name.split_once('-').map_or("", |(_, stepping)| stepping);
            ensure!(
                !stepping.is_empty() && stepping.chars().all(|c| c.is_ascii_alphanumeric()),
                "{what}: CPU model {:?} is not a product line and a stepping, such as Genoa-B1",
                model.product_name
            );
            let line = line_of(cpuid).with_context(|| {
                format!("{what}: CPU model {}: {} is in no product line", model.product_name, describe(cpuid))
            })?;
            ensure!(
                product_line(&model.product_name) == line,
                "{what}: CPU model {}: {} is certified under {line}",
                model.product_name,
                describe(cpuid)
            );
        }
        Ok(())
    }
}

/// The product line a VCEK product name carries: `Genoa` in `Genoa-B1`.
pub fn product_line(product_name: &str) -> &str {
    product_name.split_once('-').map_or(product_name, |(line, _)| line)
}

/// What a quote with `report_cpuid`, whose VCEK names `product_name`, shows its chip to be.
/// Error cases:
/// - the report is of version 2, which names no CPUID
/// - the CPUID's family and model are not certified under the VCEK's product line
pub(crate) fn identify(report_cpuid: Option<Cpuid>, product_name: String) -> anyhow::Result<Chip> {
    let cpuid = report_cpuid.context(
        "the quote is a version 2 report, which names no CPU model: a release accepts reports of version 3 or later",
    )?;
    let line = product_line(&product_name);
    ensure!(
        line_of(cpuid) == Some(line),
        "the quote's CPU, {}, is not certified under {line}, the product line of its VCEK",
        describe(cpuid)
    );
    Ok(Chip { cpuid, product_name })
}

/// The entry of `models` that `chip` is, once its TCB `reported` meets that entry's minimum.
/// Error cases:
/// - no entry names the chip's CPUID
/// - the entry names another product than the chip's VCEK
/// - `reported` is below the entry's minimum in any component
pub(crate) fn admit<'m>(chip: &Chip, reported: Tcb, models: &'m [CpuModel]) -> anyhow::Result<&'m CpuModel> {
    let model = models
        .iter()
        .find(|model| model.cpuid() == chip.cpuid)
        .with_context(|| format!("the quote's CPU, {}, is not a model the release accepts", describe(chip.cpuid)))?;
    ensure!(
        chip.product_name == model.product_name,
        "the quote's VCEK names product {}, not the {} the release lists for {}",
        chip.product_name,
        model.product_name,
        describe(chip.cpuid)
    );
    let minimum: Tcb = model.minimum_tcb.into();
    ensure!(
        reported.meets(minimum),
        "the quote's reported TCB {reported:?} is below the release minimum {minimum:?} for {}",
        model.product_name
    );
    Ok(model)
}

/// The product line the vendor certifies `cpuid`'s family and model under, if this verifier implements it.
fn line_of(cpuid: Cpuid) -> Option<&'static str> {
    (cpuid.family == FAMILY_19H)
        .then(|| PRODUCT_LINES.iter().find(|(_, models)| models.iter().any(|range| range.contains(&cpuid.model))))
        .flatten()
        .map(|(line, _)| *line)
}

/// A CPUID for messages.
fn describe(cpuid: Cpuid) -> String {
    format!("family 0x{:02x} model 0x{:02x} stepping 0x{:02x}", cpuid.family, cpuid.model, cpuid.stepping)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_cpuid_family_and_model_maps_to_the_line_the_vendor_certifies_it_under() {
        let at = |family, model| line_of(Cpuid { family, model, stepping: 0 });
        for (family, model, line) in [
            (0x19, 0x00, Some("Milan")),
            (0x19, 0x01, Some("Milan")),
            (0x19, 0x0F, Some("Milan")),
            (0x19, 0x10, Some("Genoa")),
            (0x19, 0x11, Some("Genoa")),
            (0x19, 0x1F, Some("Genoa")),
            (0x19, 0xA0, Some("Genoa")),
            (0x19, 0xAF, Some("Genoa")),
            (0x19, 0x20, None),
            (0x19, 0x9F, None),
            (0x19, 0xB0, None),
            (0x1A, 0x01, None),
            (0x17, 0x01, None),
        ] {
            assert_eq!(at(family, model), line, "family {family:#x} model {model:#x}");
        }
    }

    #[test]
    fn a_product_line_is_the_name_before_its_stepping() {
        assert_eq!(product_line("Genoa-B1"), "Genoa");
        assert_eq!(product_line("Milan-B0"), "Milan");
        assert_eq!(product_line("Genoa"), "Genoa");
    }
}
