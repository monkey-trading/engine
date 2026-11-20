//! The `engine` binary's argument parsing, tested apart from `main.rs`, which declares this module.

use super::*;

#[test]
fn custody_audit_requires_independent_signer_and_owns_the_domain() {
    assert!(Cli::try_parse_from(["engine", "replay", "--log", "public", "--custody-config", "audit.toml"]).is_err());
    let signer = "0x1111111111111111111111111111111111111111";
    assert!(
        Cli::try_parse_from([
            "engine",
            "replay",
            "--log",
            "public",
            "--custody-config",
            "audit.toml",
            "--signer",
            signer
        ])
        .is_ok()
    );
    assert!(
        Cli::try_parse_from([
            "engine",
            "replay",
            "--log",
            "public",
            "--custody-config",
            "audit.toml",
            "--signer",
            signer,
            "--chain-id",
            "31337",
            "--vault",
            signer
        ])
        .is_err()
    );
}

#[test]
fn strict_replay_without_custody_config_keeps_existing_arguments() {
    let cli = Cli::try_parse_from(["engine", "replay", "--log", "public"]).unwrap();
    assert!(matches!(cli.cmd, Cmd::Replay { custody_config: None, signer: None, .. }));
    let signer = "0x1111111111111111111111111111111111111111";
    let cli = Cli::try_parse_from([
        "engine",
        "replay",
        "--log",
        "public",
        "--chain-id",
        "31337",
        "--vault",
        signer,
        "--signer",
        signer,
    ])
    .unwrap();
    assert!(matches!(cli.cmd, Cmd::Replay { custody_config: None, chain_id: Some(31337), signer: Some(_), .. }));
}

#[test]
fn exit_proof_takes_an_account_the_house_or_a_deposit_id_but_only_one() {
    let account = "0x1111111111111111111111111111111111111111";
    let base = ["engine", "exit-proof", "--log", "public", "--seq", "7"];
    assert!(Cli::try_parse_from(base).is_err(), "a subject is required");
    let cli = Cli::try_parse_from([&base[..], &["--account", account]].concat()).unwrap();
    assert!(matches!(cli.cmd, Cmd::ExitProof { seq: 7, account: Some(_), house: false, deposit: None, .. }));
    assert!(Cli::try_parse_from([&base[..], &["--house"]].concat()).is_ok());
    let cli = Cli::try_parse_from([&base[..], &["--deposit", "3"]].concat()).unwrap();
    assert!(matches!(cli.cmd, Cmd::ExitProof { account: None, house: false, deposit: Some(3), .. }));
    assert!(Cli::try_parse_from([&base[..], &["--house", "--account", account]].concat()).is_err());
    assert!(Cli::try_parse_from([&base[..], &["--deposit", "3", "--account", account]].concat()).is_err());
    assert!(Cli::try_parse_from([&base[..], &["--deposit", "3", "--house"]].concat()).is_err());
}

#[test]
fn ct_watch_needs_a_config_and_takes_allow_mock_as_a_flag() {
    assert!(Cli::try_parse_from(["engine", "ct-watch"]).is_err());
    let cli = Cli::try_parse_from(["engine", "ct-watch", "--config", "watch.toml"]).unwrap();
    assert!(matches!(cli.cmd, Cmd::CtWatch(_)));
    assert!(Cli::try_parse_from(["engine", "ct-watch", "-c", "watch.toml", "--allow-mock"]).is_ok());
}

#[test]
fn replay_checks_run_ends_unless_every_seal_is_asked_for_with_its_workers() {
    let cli = Cli::try_parse_from(["engine", "replay", "--log", "public"]).unwrap();
    assert!(matches!(cli.cmd, Cmd::Replay { every_seal: false, replay_workers: None, .. }));
    let cli =
        Cli::try_parse_from(["engine", "replay", "--log", "public", "--every-seal", "--replay-workers", "3"]).unwrap();
    assert!(matches!(cli.cmd, Cmd::Replay { every_seal: true, replay_workers: Some(n), .. } if n.get() == 3));
    // Workers check every seal ahead, so they come only with `--every-seal`.
    assert!(Cli::try_parse_from(["engine", "replay", "--log", "public", "--replay-workers", "3"]).is_err());
}

#[test]
fn replay_in_ranges_takes_a_snapshot_directory_and_its_jobs_but_not_every_seal() {
    let cli =
        Cli::try_parse_from(["engine", "replay", "--log", "public", "--snapshots", "daily", "--jobs", "6"]).unwrap();
    assert!(matches!(cli.cmd, Cmd::Replay { snapshots: Some(_), jobs: Some(n), .. } if n.get() == 6));
    // Jobs are ranges at once, so they come only with snapshots, which check run ends.
    assert!(Cli::try_parse_from(["engine", "replay", "--log", "public", "--jobs", "6"]).is_err());
    assert!(
        Cli::try_parse_from(["engine", "replay", "--log", "public", "--snapshots", "daily", "--every-seal"]).is_err()
    );
}
