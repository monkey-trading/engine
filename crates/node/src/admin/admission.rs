//! `engine admin sign-admission`: the admission of a successor enclave's standby key, for its release,
//! signed by the key that answers for it (`handover::admission`), written to the file its node reads.
//! The key signs as itself; the release that holds the keys decides whether it answers for the
//! successor.

use std::path::PathBuf;

use anyhow::{Context, ensure};
use clap::Args;
use engine_core::eip712;
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;

use crate::config::NodeConfig;
use crate::handover::admission::Admission;
use crate::log::CheckpointDomain;

#[derive(Args)]
pub struct SignArgs {
    /// The successor's release, the keccak of its launch measurement and config hash, in hex, as the
    /// vault or the registry names it.
    #[arg(long)]
    release: String,
    /// The successor enclave's standby key, in hex, as its node logs it.
    #[arg(long)]
    key: String,
    /// Where to write the admission: `handover-admission.json` in the successor's sealing directory.
    #[arg(long)]
    out: PathBuf,
}

/// Signs the admission `args` names with `key` for `cfg`'s custody chain and vault, and writes it to
/// `args.out`.
/// Error cases:
/// - the config's vault is not an address, the release is not 32 bytes of hex, or the key not an address
/// - the file cannot be written
pub(super) fn sign(cfg: &NodeConfig, key: &SigningKey, args: &SignArgs) -> anyhow::Result<()> {
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let release = hex::decode(args.release.trim_start_matches("0x")).context("--release is not hex")?;
    ensure!(release.len() == 32, "--release is {} bytes, not the 32 of a release's keccak", release.len());
    let release: [u8; 32] = release.try_into().expect("checked to be 32 bytes");
    let standby: AccountId = args.key.parse().map_err(|e| anyhow::anyhow!("--key is not an address: {e}"))?;
    let admission = Admission::sign(key, domain, standby, release);
    let json = serde_json::to_string_pretty(&admission).context("encoding the admission")?;
    std::fs::write(&args.out, format!("{json}\n")).with_context(|| format!("writing {}", args.out.display()))?;
    println!(
        "{} admitted standby key {standby} for release 0x{}, in {}",
        eip712::address(key.verifying_key()),
        hex::encode(release),
        args.out.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests;
