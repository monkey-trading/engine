//! What every production node config holds: the template's and each config a release ships. It holds
//! what a production build requires, and each line holding a placeholder a deployment has yet to set
//! carries a trailing `# OWNER` or `# REPLACE` marker, so no placeholder is mistaken for a value. A
//! value a deployment has set carries none.

use engine_node::NodeConfig;
use engine_node::config::{CustodyProvider, FeedProvider, TeeProvider};

/// Values no deployment runs with, which a config holds until the deployment sets its own: example
/// domains, repeated-digit addresses, the operator key whose private key was discarded, and block 1.
pub const PLACEHOLDER_VALUES: [&str; 16] = [
    "example.com",
    "0x1111111111111111",
    "0x2222222222222222",
    "0x3333333333333333",
    "0x4444444444444444",
    "0x5555555555555555",
    "0x6666666666666666",
    "0x7777777777777777",
    "0x8888888888888888",
    "0x9999999999999999",
    "0xaaaaaaaaaaaaaaaa",
    "0xbbbbbbbbbbbbbbbb",
    "0xcccccccccccccccc",
    "0xdddddddddddddddd",
    "0x045a1ac079d8e8d3",
    "start_block = 1 ",
];

/// Keys whose placeholder is a value a deployment could keep, so only its marker shows it is unset.
const PLAUSIBLE_KEYS: [&str; 4] = ["product_name = ", "min_balance = ", "trusted_signers = ", "bucket = "];

/// The lines of `text` that hold one of `placeholders` with no marker, or whose marker stands alone
/// as a comment.
pub fn unmarked_placeholders(text: &str, placeholders: &[&str]) -> Vec<String> {
    let mut unmarked = Vec::new();
    for (n, line) in text.lines().enumerate() {
        // The code before any comment, ending in one space, so `start_block = 1 ` matches a bare `1`.
        let code = format!("{} ", line.split('#').next().unwrap().trim_end());
        let marked = line.contains("# OWNER") || line.contains("# REPLACE");
        let holds = placeholders.iter().any(|placeholder| code.contains(placeholder));
        let standalone = line.trim_start().starts_with("# OWNER") || line.trim_start().starts_with("# REPLACE");
        if (holds && !marked) || standalone {
            unmarked.push(format!("line {}: {line}", n + 1));
        }
    }
    unmarked
}

/// The lines of `text` marked `# REPLACE` whose value is neither one of `PLACEHOLDER_VALUES` nor under
/// one of `PLAUSIBLE_KEYS`: a placeholder the unmarked check would miss once its marker is deleted.
pub fn unlisted_placeholders(text: &str) -> Vec<String> {
    let mut unlisted = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let code = format!("{} ", line.split('#').next().unwrap().trim_end());
        let listed = PLACEHOLDER_VALUES.iter().chain(&PLAUSIBLE_KEYS).any(|placeholder| code.contains(placeholder));
        if line.contains("# REPLACE") && !line.trim_start().starts_with('#') && !listed {
            unlisted.push(format!("line {}: {line}", n + 1));
        }
    }
    unlisted
}

/// Panics unless `cfg`, loaded from `text`, holds what a production build requires of it, which
/// `NodeConfig::load` checks only when built with the `production` feature.
pub fn assert_holds_what_a_production_build_requires(cfg: &NodeConfig, text: &str) {
    assert_eq!(cfg.tee.provider, TeeProvider::ConfigfsTsm);
    assert_eq!(cfg.custody.provider, CustodyProvider::Evm);
    assert_eq!(cfg.feeds.provider, FeedProvider::Venues);
    assert!(cfg.api.tls.is_some(), "[api.tls.acme] is required");
    assert!(cfg.api.allowed_origins.iter().all(|origin| origin.starts_with("https://")));
    assert!(cfg.alerts.critical.is_some(), "real attestation needs a Critical route");
    assert!(cfg.checkpoints.is_some() && cfg.tee.sealing.is_some(), "hardware custody needs both");
    // The launch release takes quotes from Siena (EPYC 8004) alone, at AMD's floor for it.
    let models: Vec<_> = cfg.tee.cpu_models.iter().map(|model| (model.cpuid(), model.minimum_tcb)).collect();
    let siena = engine_verify::Cpuid { family: 0x19, model: 0xA0, stepping: 0x02 };
    let floor = engine_verify::MinimumTcb { boot_loader: 0, tee: 0, snp: 0x1D, microcode: 0x1B };
    assert_eq!(models, [(siena, floor)]);
    // A rotation-set member witnesses, promotes and relays heads through its own funded caller.
    assert!(cfg.log_heads.is_some() && cfg.liveness.is_some(), "[log_heads] and [liveness] run the heartbeat rule");
    let evm = cfg.custody.evm.as_ref().expect("EVM custody");
    assert!(evm.providers.len() >= 3);
    // The fresh-account budgets are the production values, the ceiling a production build enforces.
    let fresh = cfg.api.fresh_accounts;
    assert_eq!((fresh.shared.per_minute.get(), fresh.shared.burst.get()), (60, 120));
    assert_eq!((fresh.per_client.per_minute.get(), fresh.per_client.burst.get()), (12, 6));
    // The screening slots, shares and reserve are within what a production build allows.
    cfg.access.as_ref().expect("[access]").refuse_dev_screening_slots(true).expect("the production screening slots");
    // A production build reads secrets only from a delivered bundle.
    let sealing = cfg.tee.sealing.as_ref().expect("[tee.sealing]");
    assert!(sealing.secrets.is_some(), "[tee.sealing.secrets] takes the delivery");
    assert!(!text.contains("api_key_env =") && !text.contains("url_env ="), "the environment is refused");
    // Crypto from Coinbase and Kraken, FX from Massive and TraderMade, and the US shares from Massive
    // and Alpaca meet min_sources 2. The index and commodity futures, every contract, meet their
    // min_sources 1 from Massive alone (owner's decision of 2026-11-20).
    cfg.refuse_unmet_min_sources(true).expect("every launch market has its min_sources over TLS");
}
